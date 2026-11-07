# Builds the hosted demo image (packaging/demo/Dockerfile) and the two
# production stages it starts from (packaging/docker/Dockerfile) in one
# BuildKit build, from the repository root:
#
#   docker buildx bake -f packaging/demo/docker-bake.hcl --load demo
#
# The sources and runtime stages are inputs of the demo target, not images
# of their own, so the multi-gigabyte builder (its target/ directory) is
# never exported, and the storefronts compile from the sources at the same
# time as the production builder compiles the hosts. images-publish.yml
# (through .github/actions/image-build) and demo-deploy.yml build the demo
# this way.

# Link-time optimization for the demo's release builds. Shipped images keep
# Cargo.toml's fat LTO and one codegen unit; the demo trades a little runtime
# speed for a link that takes minutes instead of most of the build.
variable "RELEASE_LTO" {
  default = "thin"
}

variable "RELEASE_CODEGEN_UNITS" {
  default = "16"
}

variable "TAG" {
  default = "bookclerk-demo:local"
}

target "_production" {
  context    = "."
  dockerfile = "packaging/docker/Dockerfile"
  args = {
    RELEASE_LTO           = RELEASE_LTO
    RELEASE_CODEGEN_UNITS = RELEASE_CODEGEN_UNITS
  }
}

# The production toolchain and sources, nothing compiled yet.
target "sources" {
  inherits = ["_production"]
  target   = "sources"
}

# The production runtime stage, with the hosts the builder stage built.
target "runtime" {
  inherits = ["_production"]
  target   = "runtime"
}

target "demo" {
  context    = "packaging/demo"
  dockerfile = "Dockerfile"
  contexts = {
    sources = "target:sources"
    runtime = "target:runtime"
  }
  args = {
    SOURCES               = "sources"
    RUNTIME               = "runtime"
    RELEASE_LTO           = RELEASE_LTO
    RELEASE_CODEGEN_UNITS = RELEASE_CODEGEN_UNITS
  }
  tags = [TAG]
}
