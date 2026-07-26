# Vendored well-known protobuf types

`google/protobuf/timestamp.proto` and `google/protobuf/empty.proto`, copied
verbatim from the protobuf distribution (BSD-3-Clause, Copyright Google Inc.).

`controlplane.proto` imports both. Whether `protoc` can resolve them from its own
installation depends on how it was packaged: the macOS Homebrew build ships them
on its default include path, Debian's `protobuf-compiler` does not. Vendoring them
and adding this directory to the include path makes the build behave identically
everywhere — which is how a Docker build that worked locally and failed in the
image got fixed.

To update, re-copy them from a protobuf release matching the `prost` version in
use.
