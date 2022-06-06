#!/bin/bash
set -e
set -x

# script to download proto files for bazel remote execution

DIR=$(dirname `which $0`)

mkdir -p $DIR/build/bazel/remote/execution/v2 $DIR/build/bazel/semver
curl -o $DIR/build/bazel/remote/execution/v2/remote_execution.proto https://raw.githubusercontent.com/bazelbuild/remote-apis/main/build/bazel/remote/execution/v2/remote_execution.proto
curl -o $DIR/build/bazel/semver/semver.proto https://raw.githubusercontent.com/bazelbuild/remote-apis/main/build/bazel/semver/semver.proto

mkdir -p $DIR/google/protobuf
curl -o $DIR/google/protobuf/any.proto https://raw.githubusercontent.com/protocolbuffers/protobuf/main/src/google/protobuf/any.proto
curl -o $DIR/google/protobuf/duration.proto https://raw.githubusercontent.com/protocolbuffers/protobuf/main/src/google/protobuf/duration.proto
curl -o $DIR/google/protobuf/timestamp.proto https://raw.githubusercontent.com/protocolbuffers/protobuf/main/src/google/protobuf/timestamp.proto

mkdir -p $DIR/google/api $DIR/google/longrunning $DIR/google/rpc
curl -o $DIR/google/api/annotations.proto https://raw.githubusercontent.com/googleapis/googleapis/master/google/api/annotations.proto
curl -o $DIR/google/api/client.proto https://raw.githubusercontent.com/googleapis/googleapis/master/google/api/client.proto
curl -o $DIR/google/api/http.proto https://raw.githubusercontent.com/googleapis/googleapis/master/google/api/http.proto
curl -o $DIR/google/longrunning/operations.proto https://raw.githubusercontent.com/googleapis/googleapis/master/google/longrunning/operations.proto
curl -o $DIR/google/rpc/status.proto https://raw.githubusercontent.com/googleapis/googleapis/master/google/rpc/status.proto