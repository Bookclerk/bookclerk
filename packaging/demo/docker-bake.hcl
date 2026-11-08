# Builds the hosted demo image (packaging/demo/Dockerfile) and the two
# production stages it starts from (packaging/docker/Dockerfile) in one
# BuildKit build, from the repository root:
#
#   docker buildx bake -f packaging/demo/docker-bake.hcl --load demo
#
# The builder and runtime stages are inputs of the demo target, not images of
# their own, so the multi-gigabyte builder (its target/ directory) is never
# exported. images-publish.yml (through .github/actions/image-build) and
# demo-deploy.yml build the demo this way.

# Link-time optimization for the demo's release builds. Shipped images keep
# Cargo.toml's fat LTO and one codegen unit; the demo trades a little runtime
# speed for a link that takes minutes instead of most of the build.
variable "RELEASE_LTO" {
  default = "thin"
}

variable "RELEASE_CODEGEN_UNITS" {
  default = "16"
}

# The demo's own Rust packages, compiled in the production builder's single
# `cargo build` (package-platform --extra-package) so the dependencies they
# share with the hosts compile once instead of twice.
variable "EXTRA_PACKAGES" {
  default = "bookclerk-plugin-source-audible bookclerk-plugin-source-libro bookclerk-plugin-source-chirp bookclerk-plugin-source-graphicaudio bookclerk-demo-seed"
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
    EXTRA_PACKAGES        = EXTRA_PACKAGES
  }
}

# The production builder stage (sources and target/ with the hosts built).
target "builder" {
  inherits = ["_production"]
  target   = "builder"
}

# The production runtime stage, built from that builder.
target "runtime" {
  inherits = ["_production"]
  target   = "runtime"
}

target "demo" {
  context    = "packaging/demo"
  dockerfile = "Dockerfile"
  contexts = {
    builder = "target:builder"
    runtime = "target:runtime"
  }
  args = {
    BUILDER = "builder"
    RUNTIME = "runtime"
  }
  tags = [TAG]
}
