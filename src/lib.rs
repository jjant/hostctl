//! hostctl: see at a glance whether every Nix host (NixOS, nix-darwin,
//! home-manager) runs the configuration its flake currently describes.
//! DESIGN.md describes the architecture.

pub mod storepath;
pub mod util;
