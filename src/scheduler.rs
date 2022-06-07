use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{bail, Context};
use itertools::Itertools;
use log::{debug, error, info, warn};
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use which::which;

use crate::bazel_remote_exec::Digest;
use crate::cache::BlobDigest;
use crate::executors::ExecutionResult;
use crate::{
    bazel_remote_exec, config, Arena, Command, CommandBuilder, CommandId, File, FileId, Sandbox,
};

#[derive(Debug, PartialEq)]
pub enum ScheduleState {
    New,
    /// Command can not yet be executed because dependencies are still missing
    Waiting,
    /// Command is ready for being executed
    Ready,
    /// Command execution finished successfully
    Succeeded,
    /// Command execution failed
    Failed,
}

#[derive(Debug, PartialEq)]
pub struct SchedulerResult {
    pub succeeded: usize,
    pub failed: usize,
    pub not_run: usize,
}

type ExecutionResultChannel = (CommandId, Option<Sandbox>, ExecutionResult);

pub struct Scheduler {
    cache_enabled: bool,
    worker_threads: usize,
    /// absolute directory to resolve relative paths of input/output files
    workspace_dir: PathBuf,
    /// current working directory, read-only, used to execute commands
    current_dir: PathBuf,
    /// directory of output files
    bin_dir: PathBuf,
    files: Arena<File>,
    path_to_file_id: HashMap<PathBuf, FileId>,
    which_to_file_id: HashMap<String, FileId>,
    commands: Arena<Command>,
    waiting: HashSet<CommandId>,
    // TODO sort by weight, e.g. recursive number of rdeps
    ready: VecDeque<CommandId>,
    running: usize,
    succeeded: Vec<CommandId>,
    failed: Vec<CommandId>,
}

impl Scheduler {
    pub fn new() -> Scheduler {
        let worker_threads = num_cpus::get();
        assert!(worker_threads > 0);
        let current_dir = env::current_dir().unwrap();
        let workspace_dir = current_dir.clone();
        let bin_dir = current_dir.join(config::BIN_DIR);
        debug!("workspace_dir: {:?}", workspace_dir);
        debug!("bin_dir:       {:?}", bin_dir);
        Scheduler {
            cache_enabled: true,
            worker_threads,
            workspace_dir,
            current_dir,
            bin_dir,
            files: Default::default(),
            path_to_file_id: Default::default(),
            which_to_file_id: Default::default(),
            commands: Default::default(),
            waiting: Default::default(),
            ready: Default::default(),
            running: 0,
            succeeded: vec![],
            failed: vec![],
        }
    }

    /// Remove the binary directory
    pub fn clean(&self) {
        fs::remove_dir_all(&self.bin_dir).ok();
    }

    /// Set the directory to resolve relative paths of input/output files
    pub fn set_workspace_dir(&mut self, workspace: &Path) {
        if workspace.is_absolute() {
            self.workspace_dir = workspace.into();
        } else {
            self.workspace_dir = self.current_dir.join(workspace);
        }
        debug!("workspace_dir: {:?}", self.workspace_dir);
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn push_custom_command(
        &mut self,
        name: String,
        executable: String,
        args: Vec<String>,
        inputs: Vec<String>,
        outputs: Vec<String>,
    ) -> Result<CommandId, anyhow::Error> {
        let mut builder = CommandBuilder::new(name, args);
        builder.inputs(&inputs, self)?;
        builder.outputs(&outputs, self)?;
        builder.custom_command_executor(executable, self)?;
        self.push(builder)
    }

    pub fn push(&mut self, builder: CommandBuilder) -> Result<CommandId, anyhow::Error> {
        let id = self.commands.alloc_with_id(|id| builder.build(id));
        // TODO check if name is unique
        // patch outputs.creating_command
        for output_id in &self.commands[id].outputs {
            let output = &mut self.files[*output_id];
            assert!(output.creating_command.is_none());
            output.creating_command = Some(id);
        }
        Ok(id)
    }

    #[cfg(test)]
    pub fn get_command(&self, id: CommandId) -> Option<&Command> {
        self.commands.get(id)
    }

    pub async fn run(&mut self) -> Result<SchedulerResult, anyhow::Error> {
        if self.commands.is_empty() {
            bail!("no commands added");
        }
        self.create_dependency_graph();
        if self.cache_enabled {
            self.digest_input_files().await?;
        }
        self.create_output_dirs()?;
        let (tx, mut rx) = mpsc::channel(32);
        self.start_ready_commands(&tx);
        while self.ready.len() + self.running != 0 {
            if let Some((id, sandbox, result)) = rx.recv().await {
                self.on_command_finished(id, sandbox, result).await;
                self.start_ready_commands(&tx);
            }
        }
        Ok(SchedulerResult {
            succeeded: self.succeeded.len(),
            failed: self.failed.len(),
            not_run: self.waiting.len() + self.ready.len(),
        })
    }

    /// Register an executable to be used for a command
    pub fn executable(&mut self, arg: String) -> Result<&File, anyhow::Error> {
        if arg.contains('.') {
            self.input_file(arg)
        } else if let Some(x) = self.which_to_file_id.get(&arg) {
            Ok(&self.files[*x])
        } else {
            let path = which(&arg)?;
            info!("which({}) => {:?}", arg, path);
            let id = self.input_file(path.to_str().unwrap().into())?.id;
            self.which_to_file_id.insert(arg, id);
            Ok(&self.files[id])
        }
    }

    pub fn input_file(&mut self, arg: String) -> Result<&File, anyhow::Error> {
        let rel_path = self.rel_path(&arg)?;
        let id = self
            .path_to_file_id
            .get(&rel_path)
            .cloned()
            .unwrap_or_else(|| {
                let id = self.files.alloc_with_id(|id| File {
                    id,
                    arg,
                    path: rel_path.clone(),
                    creating_command: None,
                    digest: None,
                });
                self.path_to_file_id.insert(rel_path, id);
                id
            });
        Ok(&self.files[id])
    }

    pub fn output_file(&mut self, arg: &String) -> Result<&File, anyhow::Error> {
        let rel_path = self.rel_path(arg)?;
        if let Some(file) = self.path_to_file_id.get(&rel_path).map(|x| &self.files[*x]) {
            if let Some(creating_command) = file.creating_command {
                bail!(
                    "File {} cannot be output of multiple commands, already output of {}",
                    arg,
                    self.commands[creating_command].name
                );
            } else {
                bail!(
                    "File {} cannot be output because it's already used as data",
                    arg,
                );
            }
        }
        let id = self.files.alloc_with_id(|id| File {
            id,
            creating_command: None, // will be patched in Scheduler::push()
            path: self.bin_dir.join(&rel_path),
            arg: arg.clone(),
            digest: None,
        });
        self.path_to_file_id.insert(rel_path, id);
        Ok(&self.files[id])
    }

    /// Maps a relative path from workspace dir to cwd, allow absolute path
    fn rel_path(&self, arg: &String) -> Result<PathBuf, anyhow::Error> {
        let path = Path::new(arg);
        if path.is_absolute() {
            Ok(PathBuf::from(
                path.strip_prefix(&self.current_dir).unwrap_or(path),
            ))
        } else {
            self.workspace_dir
                .join(path)
                .strip_prefix(&self.current_dir)
                .map(PathBuf::from)
                .with_context(|| {
                    format!(
                        "File is not within cwd ({:?}): {:?}",
                        self.current_dir, path
                    )
                })
        }
    }

    fn create_dependency_graph(&mut self) {
        self.waiting.reserve(self.commands.len());
        self.succeeded.reserve(self.commands.len());
        let mut rdeps = vec![];
        for command in self.commands.iter_mut() {
            assert_eq!(command.schedule_state, ScheduleState::New);
            for input_id in &command.inputs {
                if let Some(dep) = self.files[*input_id].creating_command {
                    command.unfinished_deps.push(dep);
                    rdeps.push((dep, command.id));
                }
            }
            if command.unfinished_deps.is_empty() {
                command.schedule_state = ScheduleState::Ready;
                self.ready.push_back(command.id);
            } else {
                command.schedule_state = ScheduleState::Waiting;
                self.waiting.insert(command.id);
            }
        }
        for (id, rdep) in rdeps {
            self.commands[id].reverse_deps.push(rdep);
        }
        self.check_for_circular_dependencies();
        assert!(!self.ready.is_empty());
    }

    fn check_for_circular_dependencies(&self) {
        // TODO
    }

    async fn digest_input_files(&mut self) -> Result<(), anyhow::Error> {
        let concurrent = self.worker_threads;
        let (tx, mut rx) = mpsc::channel(concurrent);
        let mut tx_option = Some(tx);
        let mut next_file_id = self.files.first_id();
        for _ in 0..concurrent {
            self.spawn_digest_input_file(&mut next_file_id, &mut tx_option);
        }
        let mut missing_files = 0;
        while let Some((id, result)) = rx.recv().await {
            match result {
                Ok(digest) => {
                    self.files[id].digest = Some(digest);
                }
                Err(x) => {
                    warn!("{}", x);
                    missing_files += 1;
                }
            };
            self.spawn_digest_input_file(&mut next_file_id, &mut tx_option);
        }
        if missing_files != 0 {
            bail!("{missing_files} input files not found!");
        }
        Ok(())
    }

    fn spawn_digest_input_file(
        &self,
        next_id: &mut FileId,
        tx_option: &mut Option<Sender<(FileId, Result<BlobDigest, anyhow::Error>)>>,
    ) {
        if tx_option.is_none() {
            return;
        }
        while let Some(file) = self.files.get_and_inc_id(next_id) {
            if file.creating_command.is_none() {
                let id = file.id;
                let path = file.path.clone();
                let tx = tx_option.clone().unwrap();
                tokio::spawn(async move {
                    tx.send((id, Digest::for_file(path).await)).await.ok();
                });
                return;
            }
        }
        tx_option.take();
    }

    fn create_output_dirs(&self) -> Result<(), anyhow::Error> {
        let dirs = self
            .files
            .iter()
            .map(|x| x.path.parent().unwrap())
            .sorted_unstable()
            .dedup();
        for x in dirs {
            fs::create_dir_all(x)
                .with_context(|| format!("Failed to create output directory: {:?}", x.clone()))?;
        }
        Ok(())
    }

    fn start_ready_commands(&mut self, tx: &Sender<ExecutionResultChannel>) {
        while self.running < self.worker_threads && !self.ready.is_empty() {
            let id = self.ready.pop_front().unwrap();
            self.start_next_command(id, tx.clone());
        }
    }

    fn start_next_command(&mut self, id: CommandId, tx: Sender<ExecutionResultChannel>) {
        self.running += 1;
        let command = &self.commands[id];
        assert_eq!(command.schedule_state, ScheduleState::Ready);
        assert_eq!(command.unfinished_deps.len(), 0);
        //let action = self.get_bzl_action_for_command(command);
        info!(
            "Execute {}: {}",
            command.name,
            command.executor.command_line()
        );
        let executor = command.executor.clone();
        let sandbox = executor
            .use_sandbox()
            .then(|| Sandbox::new(command, &self.files));
        tokio::task::spawn(async move {
            if let Some(sandbox) = &sandbox {
                sandbox
                    .create_and_provide_inputs()
                    .await
                    .with_context(|| executor.command_line())
                    .unwrap();
            }
            let result = executor.exec(sandbox.as_ref().map(|x| x.dir.clone())).await;
            // TODO .with_context(|| format!("{}\n{}", command.name, command.command_line()))?;
            tx.send((id, sandbox, result)).await.unwrap();
        });
    }

    async fn on_command_finished(
        &mut self,
        id: CommandId,
        sandbox: Option<Sandbox>,
        result: ExecutionResult,
    ) {
        self.running -= 1;
        if let Some(sandbox) = sandbox {
            sandbox
                .handle_outputs_and_destroy()
                .await
                .with_context(|| self.commands[id].executor.command_line())
                .with_context(|| self.commands[id].name.clone())
                .unwrap();
        }
        if result.success() {
            self.on_command_succeeded(id, result);
        } else {
            self.on_command_failed(id, result);
        }
    }

    /// Track state and check if reverse dependencies are ready
    fn on_command_succeeded(&mut self, id: CommandId, result: ExecutionResult) {
        self.succeeded.push(id);
        let command = &mut self.commands[id];
        command.schedule_state = ScheduleState::Succeeded;
        info!("Success {}: {:?}", command.name, result);
        for rdep_id in command.reverse_deps.clone() {
            let rdep = &mut self.commands[rdep_id];
            assert_eq!(rdep.schedule_state, ScheduleState::Waiting);
            assert!(!rdep.unfinished_deps.is_empty());
            rdep.unfinished_deps
                .swap_remove(rdep.unfinished_deps.iter().position(|x| *x == id).unwrap());
            if rdep.unfinished_deps.is_empty() {
                rdep.schedule_state = ScheduleState::Ready;
                self.waiting.remove(&rdep_id);
                self.ready.push_back(rdep_id);
            }
        }
    }

    fn on_command_failed(&mut self, id: CommandId, result: ExecutionResult) {
        self.failed.push(id);
        let command = &self.commands[id];
        error!("Error  {}: {:?}", command.name, result);
    }

    fn get_bzl_action_for_command(&self, command: &Command) -> bazel_remote_exec::Action {
        let bzl_command = bazel_remote_exec::Command {
            arguments: command.executor.args_with_executable(),
            environment_variables: vec![],
            output_paths: command
                .outputs
                .iter()
                .map(|x| self.files[*x].path.to_str().unwrap())
                .sorted_unstable()
                .dedup()
                .map_into()
                .collect(),
            working_directory: "".to_string(),
            ..Default::default()
        };
        // TODO properly build bazel_remote_exec::Directory tree
        let bzl_input_root = bazel_remote_exec::Directory {
            files: command
                .inputs
                .iter()
                .map(|x| {
                    let file = &self.files[*x];
                    assert!(file.digest.is_some());
                    bazel_remote_exec::FileNode {
                        name: file.path.to_str().unwrap().into(),
                        digest: file.digest.clone(),
                        is_executable: false, // TODO bazel_remote_exec::FileNode::is_executable
                        node_properties: None,
                    }
                })
                .sorted_unstable_by(|a, b| Ord::cmp(&a.name, &b.name))
                .collect(),
            directories: vec![],
            symlinks: vec![],
            node_properties: None,
        };
        let bzl_action = bazel_remote_exec::Action {
            command_digest: Some(Digest::for_message(&bzl_command)),
            input_root_digest: Some(Digest::for_message(&bzl_input_root)),
            ..Default::default()
        };
        bzl_action
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use approx::assert_abs_diff_eq;

    use crate::Scheduler;

    /// Test that commands are actually run in parallel limited by Scheduler::worker_threads
    #[tokio::test]
    async fn parallel() {
        let mut scheduler = Scheduler::new();
        scheduler.cache_enabled = false; // calculating digest of cmake takes too long in test config to check duration
        let threads = scheduler.worker_threads;
        let n = threads * 3;
        let sleep_duration = 0.5;
        for i in 0..n {
            scheduler
                .push_custom_command(
                    format!("{}", i),
                    "cmake".into(),
                    vec!["-E".into(), "sleep".into(), sleep_duration.to_string()],
                    vec![],
                    vec![],
                )
                .unwrap();
        }
        assert_eq!(scheduler.len(), n);
        let start = Instant::now();
        scheduler.run().await.unwrap();
        let duration = start.elapsed();
        assert_eq!(scheduler.succeeded.len(), n);
        assert_abs_diff_eq!(
            duration.as_secs_f64(),
            (n as f64 / threads as f64).ceil() * sleep_duration,
            epsilon = sleep_duration * 0.5
        );
    }
}
