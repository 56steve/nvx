//! Tests of the kernel command line as Linux reads it. The expectations follow Linux 6.18:
//! `lib/cmdline.c`, `kernel/params.c`, `lib/vsprintf.c`, `lib/kstrtox.c`, and
//! `drivers/virtio/virtio_mmio.c`.

use super::*;

/// The MMIO base of the scratch block device of a layered root.
const SCRATCH: u64 = 0xd000_6000;

/// Checks that the kernel splits `command_line` into the parameters `expected`.
fn assert_parameters(command_line: &[u8], expected: &[(&str, Option<&str>)]) {
    let expected: Vec<_> = expected
        .iter()
        .map(|&(name, value)| Parameter {
            name: name.as_bytes(),
            value: value.map(str::as_bytes),
        })
        .collect();
    assert_eq!(
        parameters(command_line).collect::<Vec<_>>(),
        expected,
        "{}",
        command_line.escape_ascii()
    );
}

/// Returns the device of `size` bytes at `base` with `irq`, and with `id` if any.
fn device(size: u64, base: u64, irq: u32, id: Option<i32>) -> Option<VirtioMmioDevice> {
    Some(VirtioMmioDevice {
        size,
        base,
        irq,
        id,
    })
}

#[test]
fn whitespace_is_what_the_kernel_takes_for_it() {
    for byte in 0..=u8::MAX {
        let expected = matches!(byte, 0x09..=0x0d | 0x20 | 0xa0);
        assert_eq!(is_space(byte), expected, "{byte:#04x}");
    }
}

#[test]
fn command_lines_split_into_parameters_as_the_kernel_splits_them() {
    assert_parameters(b"a b=c", &[("a", None), ("b", Some("c"))]);
    assert_parameters(
        b" a\x0bb\x0cc\rd\xa0e\tf\ng=h ",
        &[
            ("a", None),
            ("b", None),
            ("c", None),
            ("d", None),
            ("e", None),
            ("f", None),
            ("g", Some("h")),
        ],
    );
    assert_parameters(b"", &[]);
    assert_parameters(b" \t\n", &[]);
    assert_parameters(b"\"a b\" c", &[("a b", None), ("c", None)]);
    assert_parameters(b"\"a=b c\" d", &[("a", Some("b c")), ("d", None)]);
    assert_parameters(b"a=\"b c\" d", &[("a", Some("b c")), ("d", None)]);
    assert_parameters(b"a=\"b c", &[("a", Some("b c"))]);
    assert_parameters(b"a=\"\" b=c\"", &[("a", Some("")), ("b", Some("c\""))]);
    assert_parameters(b"a=\"", &[("a", Some(""))]);
    assert_parameters(b"x=\"a b=c\"", &[("x", Some("a b=c"))]);
    assert_parameters(b"a\"b c\"d e", &[("a\"b c\"d", None), ("e", None)]);
    assert_parameters(b"=a=b", &[("=a", Some("b"))]);
    assert_parameters(b"a=", &[("a", Some(""))]);
    assert_parameters(b"\"", &[("", None)]);
    // The kernel stops at a NUL, and at a `--` without a value.
    assert_parameters(b"a\0b", &[("a", None)]);
    assert_parameters(b"a -- b", &[("a", None)]);
    assert_parameters(b"a \"--\" b", &[("a", None)]);
    assert_parameters(b"a --=1 b", &[("a", None), ("--", Some("1")), ("b", None)]);
    assert_parameters(b"a ---- b", &[("a", None), ("----", None), ("b", None)]);
    // Once it stopped, it stays stopped.
    let mut split = parameters(b"a -- b");
    assert_eq!(
        split.next().map(|parameter| parameter.name),
        Some(&b"a"[..])
    );
    assert_eq!(split.next(), None);
    assert_eq!(split.next(), None);
}

#[test]
fn parameter_names_compare_as_the_kernel_compares_them() {
    let named = |name: &str, expected: &str| {
        Parameter {
            name: name.as_bytes(),
            value: None,
        }
        .is_named(expected)
    };
    for name in ["virtio_mmio.device", "virtio-mmio.device"] {
        assert!(named(name, VIRTIO_MMIO_DEVICE), "{name}");
    }
    assert!(named("a_b", "a-b"));
    assert!(named("a-b", "a_b"));
    for name in [
        "virtio-mmio-device",
        "virtio_mmio_device",
        "virtio_mmio.devices",
        "virtio.mmio.device",
        "Virtio_mmio.device",
        "",
    ] {
        assert!(!named(name, VIRTIO_MMIO_DEVICE), "{name}");
    }
}

#[test]
fn device_values_read_as_the_driver_reads_them() {
    for (value, expected) in [
        ("0x1000@0xd0006000:11", device(0x1000, SCRATCH, 11, None)),
        ("4K@0XD0006000:11:3", device(4096, SCRATCH, 11, Some(3))),
        ("4k@0xd0006000:11", device(4096, SCRATCH, 11, None)),
        ("1G@0xd0006000:11", device(1 << 30, SCRATCH, 11, None)),
        ("2e@0xd0006000:11", device(2 << 60, SCRATCH, 11, None)),
        ("0x1E@0xd0006000:11", device(0x1e, SCRATCH, 11, None)),
        ("4096@3489685504:11", device(4096, SCRATCH, 11, None)),
        ("4096@032000060000:11", device(4096, SCRATCH, 11, None)),
        ("010@0xd0006000:11", device(8, SCRATCH, 11, None)),
        ("0x1000@ \t0xd0006000:11", device(0x1000, SCRATCH, 11, None)),
        ("0x1000@0xd0003000:4", device(0x1000, 0xd000_3000, 4, None)),
        // `memparse` takes a suffix even without digits, and reads no size at all as 0. The
        // kernel then fails to claim the empty region, which the driver's reading leaves to it.
        ("K@0xd0006000:11", device(0, SCRATCH, 11, None)),
        ("@0xd0006000:11", device(0, SCRATCH, 11, None)),
        // A size wraps at 64 bits, and so does a base, which a minus sign negates.
        ("16E@0xd0006000:11", device(0, SCRATCH, 11, None)),
        (
            "0x1000@-0xffffffff2fffa000:11",
            device(0x1000, SCRATCH, 11, None),
        ),
        (
            "0x1000@0x100000000d0006000:11",
            device(0x1000, SCRATCH, 11, None),
        ),
        ("0x1000@0x10000000000000000:11", device(0x1000, 0, 11, None)),
        ("0x1000@-1:11", device(0x1000, u64::MAX, 11, None)),
        // IRQ and ID are decimal, after any whitespace, and keep their low 32 bits.
        ("0x1000@0xd0006000: 11", device(0x1000, SCRATCH, 11, None)),
        ("0x1000@0xd0006000:011", device(0x1000, SCRATCH, 11, None)),
        (
            "0x1000@0xd0006000:4294967307",
            device(0x1000, SCRATCH, 11, None),
        ),
        (
            "0x1000@0xd0006000:11:-2",
            device(0x1000, SCRATCH, 11, Some(-2)),
        ),
        (
            "0x1000@0xd0006000:11:\t7",
            device(0x1000, SCRATCH, 11, Some(7)),
        ),
        (
            "0x1000@0xd0006000:11:010",
            device(0x1000, SCRATCH, 11, Some(10)),
        ),
        (
            "0x1000@0xd0006000:11:4294967303",
            device(0x1000, SCRATCH, 11, Some(7)),
        ),
        (
            "0x1000@0xd0006000:11:2147483648",
            device(0x1000, SCRATCH, 11, Some(i32::MIN)),
        ),
        // The driver refuses an IRQ of 0, a value without an IRQ, and text after the last integer.
        ("0x1000@0xd0006000:0", None),
        ("0x1000@0xd0006000:4294967296", None),
        ("0x1000@0xd0006000", None),
        ("0x1000@0xd0006000:", None),
        ("0x1000@0xd0006000:-11", None),
        ("0x1000@0xd0006000:+11", None),
        ("0x1000@0xd0006000:0x11", None),
        ("0x1000@0xd0006000:11x", None),
        ("0x1000@0xd0006000:11 ", None),
        ("0x1000@0xd0006000:11:", None),
        ("0x1000@0xd0006000:11:x", None),
        ("0x1000@0xd0006000:11:-", None),
        ("0x1000@0xd0006000:11:7x", None),
        ("0x1000@0xd0006000:11:2:3", None),
        // It needs `@` right after the size, and a base that starts with a digit.
        ("0xd0006000:11", None),
        ("0x1000@:11", None),
        ("0x1000@0x:11", None),
        ("0x1000@-:11", None),
        ("0x1000@--1:11", None),
        ("0x1000@+3489685504:11", None),
        ("0x1000@0xd0006000x:11", None),
        ("0x1000@0xd0006000 :11", None),
        ("0x1000@09:11", None),
        ("0x1000 @0xd0006000:11", None),
        (" 0x1000@0xd0006000:11", None),
        ("-0x1000@0xd0006000:11", None),
        ("4KB@0xd0006000:11", None),
        ("x@0xd0006000:11", None),
        ("", None),
    ] {
        assert_eq!(
            VirtioMmioDevice::parse(value.as_bytes()),
            expected,
            "{value}"
        );
    }
}

#[test]
fn devices_come_from_the_parameters_that_the_kernel_reads() {
    let command_line = b"quiet virtio_mmio.device=0x1000@0xd0003000:4 \
        virtio-mmio.device=4K@0xd0006000:11:3 \"virtio_mmio.device=0x1000@0xd0007000:3\" \
        virtio_mmio.device virtio_mmio.device=0x1000@0xd0008000:0 \
        virtio_mmio_device=0x1000@0xd0009000:5 x=\"virtio_mmio.device=0x1000@0xd000a000:6\" \
        virtio_mmio.device=\"0x1000@ 0xd000b000:7\" -- virtio_mmio.device=0x1000@0xd000c000:8";
    let devices: Vec<_> = virtio_mmio_devices(command_line).collect();
    assert_eq!(
        devices,
        [
            device(0x1000, 0xd000_3000, 4, None).unwrap(),
            device(4096, SCRATCH, 11, Some(3)).unwrap(),
            device(0x1000, 0xd000_7000, 3, None).unwrap(),
            device(0x1000, 0xd000_b000, 7, None).unwrap(),
        ]
    );
    assert_eq!(virtio_mmio_devices(b"").count(), 0);
}
