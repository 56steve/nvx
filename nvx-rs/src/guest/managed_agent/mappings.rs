use std::ffi::{CStr, OsString};
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::guest::mappings::{Access, InvalidMapping, Mapping};
use crate::guest::sys::{self, Errno, Mode, MountFlags};
use crate::protocol::{self, GuestHostMap};

use super::seam::{System, path_c_string};

/// Progress and latched failure state of the boot-announced host mapping table.
#[derive(Default)]
pub(crate) struct HostMappings {
    /// Number of mappings announced on the kernel command line.
    pub(crate) expected: u32,
    /// Number of mappings mounted in order.
    pub(crate) applied: u32,
    /// Whether a prior mount failure permanently refused the remaining table.
    pub(crate) failed: bool,
}

/// Failure returned while validating or applying one MAPS request.
#[derive(Debug)]
pub(crate) enum MappingError {
    /// The payload or sequence does not match the announced table.
    Invalid,
    /// A specific mapping failed while being mounted.
    Mount {
        /// Errno returned by mount preparation or the mount itself.
        status: i32,
        /// Zero-based mapping-table entry that failed.
        entry: u32,
    },
}

/// Reads the announced mapping count and validates the host export mount.
pub(crate) fn prepare(system: &mut impl System, mappings: &mut HostMappings) -> Result<(), Errno> {
    let mut command_line = [0u8; 4096];
    let count = std::fs::File::open(&system.paths().proc_cmdline)
        .and_then(|mut file| file.read(&mut command_line[..4095]))
        .map_err(|error| sys::errno_of(&error))?;
    mappings.expected =
        protocol::parse_host_mapping_count(&command_line[..count]).map_err(|_| Errno::EINVAL)?;
    let hostfs = system.paths().hostfs_dir.clone();
    let root = hostfs.join("root");
    let mounted = match (std::fs::metadata(&hostfs), std::fs::metadata(&root)) {
        (Ok(parent), Ok(export)) => parent.dev() != export.dev(),
        _ => false,
    };
    if mounted {
        let hostfs = path_c_string(&hostfs)?;
        system.chown(&hostfs, 0, 0)?;
        system.chmod(&hostfs, Mode::from_bits_retain(0o700))?;
    }
    if mappings.expected != 0 && !mounted {
        return Err(Errno::ENOENT);
    }
    Ok(())
}

/// Validates and applies one ordered batch of host mappings.
///
/// The agent keeps the table's bookkeeping: the count that the kernel command line announced,
/// the order of the batches, and the latch after a failure. [`Mapping`] applies each entry, with
/// the host export's mount, `hostfs_dir/root`, as the export root and the agent's own root, `/`,
/// as the target root.
pub(crate) fn apply_request(
    system: &mut impl System,
    mappings: &mut HostMappings,
    payload: &[u8],
) -> Result<(), MappingError> {
    if mappings.failed {
        return Err(MappingError::Invalid);
    }
    let request =
        protocol::decode_guest_maps_payload(payload).map_err(|_| MappingError::Invalid)?;
    let count = u32::try_from(request.maps.len()).map_err(|_| MappingError::Invalid)?;
    if request.first != mappings.applied
        || count == 0
        || mappings.applied > mappings.expected
        || count > mappings.expected - mappings.applied
    {
        return Err(MappingError::Invalid);
    }
    let first = request.first;
    let batch = request
        .maps
        .into_iter()
        .map(mapping_of)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MappingError::Invalid)?;
    let export_root = system.paths().hostfs_dir.join("root");
    for (offset, mapping) in batch.iter().enumerate() {
        let mount = |source: Option<&CStr>, target: &CStr, flags: MountFlags| {
            system.mount(source, target, None, flags)
        };
        if let Err(failure) = mapping.apply_with(&export_root, Path::new("/"), mount) {
            mappings.failed = true;
            return Err(MappingError::Mount {
                status: failure.raw_os_error(),
                entry: first + offset as u32,
            });
        }
        mappings.applied += 1;
    }
    Ok(())
}

/// Returns the library's mapping of one decoded entry, which the decoder's path rules let
/// through unchanged.
///
/// The paths are the decoder's buffers, which it bounds: it refuses a payload of more than
/// `MAX_HOST_MAPPINGS` entries before it allocates them, and a source or target of more than
/// `MAX_MAP_PATH_BYTES` bytes.
fn mapping_of(entry: GuestHostMap) -> Result<Mapping, InvalidMapping> {
    let path = |bytes: Vec<u8>| PathBuf::from(OsString::from_vec(bytes));
    let access = if entry.read_only {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };
    Mapping::new(path(entry.source), path(entry.target), access)
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::super::seam::TestSystem;
    use super::*;

    fn setup(expected: u32) -> (tempfile::TempDir, TestSystem, HostMappings) {
        let (temporary, system) = TestSystem::with_host_export();
        (
            temporary,
            system,
            HostMappings {
                expected,
                ..HostMappings::default()
            },
        )
    }

    fn payload(first: u32, entries: &[(&[u8], &[u8], u16)], count: Option<u32>) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&first.to_le_bytes());
        payload.extend_from_slice(&count.unwrap_or(entries.len() as u32).to_le_bytes());
        for (source, target, flags) in entries {
            payload.extend_from_slice(&flags.to_le_bytes());
            payload.extend_from_slice(&(source.len() as u16).to_le_bytes());
            payload.extend_from_slice(&(target.len() as u16).to_le_bytes());
            payload.extend_from_slice(source);
            payload.extend_from_slice(target);
        }
        payload
    }

    #[test]
    fn mappings_mount_in_order_and_unblock_workloads() {
        let (temporary, mut system, mut mappings) = setup(3);
        let guest = temporary.path().join("guest");
        let source = guest.join("src");
        let notes = guest.join("src/notes.txt");
        let out = guest.join("out");
        let first = payload(
            0,
            &[
                (
                    b"0/src",
                    source.as_os_str().as_bytes(),
                    protocol::MAP_READ_ONLY,
                ),
                (b"0/notes.txt", notes.as_os_str().as_bytes(), 0),
            ],
            None,
        );
        apply_request(&mut system, &mut mappings, &first).unwrap();
        assert_eq!((mappings.applied, mappings.expected), (2, 3));
        let second = payload(2, &[(b"1", out.as_os_str().as_bytes(), 0)], None);
        apply_request(&mut system, &mut mappings, &second).unwrap();
        assert_eq!((mappings.applied, mappings.expected), (3, 3));
        assert_eq!(system.mounts.len(), 6);
        assert_eq!(
            system.mounts[1].2,
            MountFlags::MS_BIND
                | MountFlags::MS_REMOUNT
                | MountFlags::MS_NOSUID
                | MountFlags::MS_NODEV
                | MountFlags::MS_RDONLY
        );
        assert!(source.is_dir());
        assert!(notes.is_file());
        assert!(out.is_dir());
    }

    #[test]
    fn entries_beyond_the_announcement_or_out_of_order_are_refused() {
        let (temporary, mut system, mut mappings) = setup(1);
        let target = temporary.path().join("guest/src");
        let entry = [(b"0/src".as_slice(), target.as_os_str().as_bytes(), 0)];
        for invalid in [payload(1, &entry, None), payload(0, &entry, Some(2))] {
            assert!(matches!(
                apply_request(&mut system, &mut mappings, &invalid),
                Err(MappingError::Invalid)
            ));
        }
        apply_request(&mut system, &mut mappings, &payload(0, &entry, None)).unwrap();
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &payload(1, &entry, None)),
            Err(MappingError::Invalid)
        ));
        assert_eq!(system.mounts.len(), 2);
    }

    #[test]
    fn malformed_tables_mount_nothing() {
        let (temporary, mut system, mut mappings) = setup(2);
        let target = temporary.path().join("guest/x");
        let good = (b"0/src".as_slice(), target.as_os_str().as_bytes(), 0);
        let malformed = [
            payload(
                0,
                &[good, (b"0/../1", target.as_os_str().as_bytes(), 0)],
                None,
            ),
            payload(
                0,
                &[good, (b"/0/src", target.as_os_str().as_bytes(), 0)],
                None,
            ),
            payload(0, &[good, (b"0/src", b"relative", 0)], None),
            payload(0, &[good, (b".", target.as_os_str().as_bytes(), 0)], None),
            payload(
                0,
                &[good, (b"0//src", target.as_os_str().as_bytes(), 0)],
                None,
            ),
            payload(
                0,
                &[good, (b"0/s\0rc", target.as_os_str().as_bytes(), 0)],
                None,
            ),
            payload(
                0,
                &[good, (b"0/src", target.as_os_str().as_bytes(), 2)],
                None,
            ),
            payload(0, &[good, (b"", target.as_os_str().as_bytes(), 0)], None),
            [payload(0, &[good], None), vec![0]].concat(),
            payload(0, &[good], Some(2)),
            payload(0, &[], Some(0)),
            vec![0, 0, 0],
        ];
        for payload in malformed {
            assert!(matches!(
                apply_request(&mut system, &mut mappings, &payload),
                Err(MappingError::Invalid)
            ));
        }
        assert!(system.mounts.is_empty());
        assert_eq!(mappings.applied, 0);
    }

    #[test]
    fn without_an_announcement_tables_are_refused() {
        let (temporary, mut system, mut mappings) = setup(0);
        let target = temporary.path().join("guest/src");
        let request = payload(0, &[(b"0/src", target.as_os_str().as_bytes(), 0)], None);
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Invalid)
        ));
        assert!(system.mounts.is_empty());
    }

    #[test]
    fn a_failed_mount_latches_and_keeps_refusing_tables() {
        let (temporary, mut system, mut mappings) = setup(2);
        system.fail_mount_call = Some(1);
        let target = temporary.path().join("guest/src");
        let request = payload(0, &[(b"0/src", target.as_os_str().as_bytes(), 0)], None);
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Mount { status, entry: 0 }) if status == Errno::ENOENT as i32
        ));
        assert!(mappings.failed);
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Invalid)
        ));
        assert_eq!(mappings.applied, 0);
    }

    #[test]
    fn a_failure_in_the_middle_of_a_batch_keeps_earlier_mappings_and_latches() {
        // The second entry fails at its bind, the third mount call, or at its missing source.
        for (fail_mount_call, source, mounted) in [
            (Some(3), b"0/notes.txt".as_slice(), 3),
            (None, b"0/missing".as_slice(), 2),
        ] {
            let (temporary, mut system, mut mappings) = setup(3);
            system.fail_mount_call = fail_mount_call;
            let guest = temporary.path().join("guest");
            let [first, second, third] = ["first", "second", "third"].map(|name| guest.join(name));
            let request = payload(
                0,
                &[
                    (
                        b"0/src",
                        first.as_os_str().as_bytes(),
                        protocol::MAP_READ_ONLY,
                    ),
                    (source, second.as_os_str().as_bytes(), 0),
                    (b"1", third.as_os_str().as_bytes(), 0),
                ],
                None,
            );
            assert!(matches!(
                apply_request(&mut system, &mut mappings, &request),
                Err(MappingError::Mount { status, entry: 1 }) if status == Errno::ENOENT as i32
            ));
            assert_eq!((mappings.applied, mappings.failed), (1, true));
            // The first mapping stays mounted read-only, and the third is never attempted.
            assert_eq!(system.mounts.len(), mounted);
            assert_eq!(
                system.mounts[1],
                (
                    None,
                    first.as_os_str().as_bytes().to_vec(),
                    MountFlags::MS_BIND
                        | MountFlags::MS_REMOUNT
                        | MountFlags::MS_NOSUID
                        | MountFlags::MS_NODEV
                        | MountFlags::MS_RDONLY
                )
            );
            assert!(first.is_dir());
            // The latch refuses the rest of the table, wherever it continues.
            for continued in [1, 2] {
                let rest = payload(continued, &[(b"1", third.as_os_str().as_bytes(), 0)], None);
                assert!(matches!(
                    apply_request(&mut system, &mut mappings, &rest),
                    Err(MappingError::Invalid)
                ));
            }
            assert_eq!(system.mounts.len(), mounted);
            assert_eq!(mappings.applied, 1);
            assert!(!third.exists());
        }
    }

    #[test]
    fn every_entry_that_the_decoder_accepts_becomes_a_library_mapping() {
        assert_eq!(
            crate::guest::mappings::MAX_PATH_BYTES,
            protocol::MAX_MAP_PATH_BYTES
        );
        let longest = vec![b'a'; protocol::MAX_MAP_PATH_BYTES];
        let target = [b"/".as_slice(), &longest[1..]].concat();
        let request = protocol::decode_guest_maps_payload(&payload(
            0,
            &[
                (
                    longest.as_slice(),
                    target.as_slice(),
                    protocol::MAP_READ_ONLY,
                ),
                (b"0/\xff", b"/\xfe", 0),
            ],
            None,
        ))
        .unwrap();
        let converted = request
            .maps
            .into_iter()
            .map(mapping_of)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let parts = |mapping: &Mapping| {
            (
                mapping.source().as_os_str().as_bytes().to_vec(),
                mapping.target().as_os_str().as_bytes().to_vec(),
                mapping.access(),
            )
        };
        assert_eq!(
            converted.iter().map(parts).collect::<Vec<_>>(),
            [
                (longest, target[1..].to_vec(), Access::ReadOnly),
                (b"0/\xff".to_vec(), b"\xfe".to_vec(), Access::ReadWrite),
            ]
        );
    }
}
