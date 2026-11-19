use std::path::PathBuf;

pub mod starting_binary;

/// Retrieves the currently running binary's path, taking into account security considerations.
///
/// The path is cached as soon as possible (before even `main` runs) and that value is returned
/// repeatedly instead of fetching the path every time. It is possible for the path to not be found,
/// or explicitly disabled (see following macOS specific behavior).
///
/// # Platform-specific behavior
///
/// On `macOS`, this function will return an error if the original path contained any symlinks
/// due to less protection on macOS regarding symlinks. This behavior can be disabled by setting the
/// `process-relaunch-dangerous-allow-symlink-macos` feature, although it is *highly discouraged*.
///
/// # Security
///
/// If the above platform-specific behavior does **not** take place, this function uses the
/// following resolution.
///
/// We canonicalize the path we received from [`std::env::current_exe`] to resolve any soft links.
/// This avoids the usual issue of needing the file to exist at the passed path because a valid
/// current executable result for our purpose should always exist. Notably,
/// [`std::env::current_exe`] also has a security section that goes over a theoretical attack using
/// hard links. Let's cover some specific topics that relate to different ways an attacker might
/// try to trick this function into returning the wrong binary path.
///
/// ## Symlinks ("Soft Links")
///
/// [`std::path::Path::canonicalize`] is used to resolve symbolic links to the original path,
/// including nested symbolic links (`link2 -> link1 -> bin`). On macOS, any results that include
/// a symlink are rejected by default due to lesser symlink protections. This can be disabled,
/// **although discouraged**, with the `process-relaunch-dangerous-allow-symlink-macos` feature.
///
/// ## Hard Links
///
/// A [Hard Link] is a named entry that points to a file in the file system.
/// On most systems, this is what you would think of as a "file". The term is
/// used on filesystems that allow multiple entries to point to the same file.
/// The linked [Hard Link] Wikipedia page provides a decent overview.
///
/// In short, unless the attacker was able to create the link with elevated
/// permissions, it should generally not be possible for them to hard link
/// to a file they do not have permissions to - with exception to possible
/// operating system exploits.
///
/// There are also some platform-specific information about this below.
///
/// ### Windows
///
/// Windows requires a permission to be set for the user to create a symlink
/// or a hard link, regardless of ownership status of the target. Elevated
/// permissions users have the ability to create them.
///
/// ### macOS
///
/// macOS allows for the creation of symlinks and hard links to any file.
/// Accessing through those links will fail if the user who owns the links
/// does not have the proper permissions on the original file.
///
/// ### Linux
///
/// Linux allows for the creation of symlinks to any file. Accessing the
/// symlink will fail if the user who owns the symlink does not have the
/// proper permissions on the original file.
///
/// Linux additionally provides a kernel hardening feature since version
/// 3.6 (30 September 2012). Most distributions since then have enabled
/// the protection (setting `fs.protected_hardlinks = 1`) by default, which
/// means that a vast majority of desktop Linux users should have it enabled.
/// **The feature prevents the creation of hardlinks that the user does not own
/// or have read/write access to.** [See the patch that enabled this].
///
/// [Hard Link]: https://en.wikipedia.org/wiki/Hard_link
/// [See the patch that enabled this]: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/commit/?id=800179c9b8a1e796e441674776d11cd4c05d61d7
pub fn current_exe() -> std::io::Result<PathBuf> {
  starting_binary::STARTING_BINARY.cloned()
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn test_current_exe() {
    let current_exe = current_exe().unwrap();
    assert!(current_exe.exists())
  }
}
