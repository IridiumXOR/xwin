//! Verifies that the WDK acquired from nuget can actually be used to compile and
//! link a kernel mode driver.
//!
//! Note this is deliberately not part of `compiles.rs`, as it needs its own splat
//! output and downloads an additional ~100MiB.

use std::process::Command;

const KMDF_VERSION: &str = "1.35";

fn splat_root() -> xwin::PathBuf {
    let ctx = xwin::Ctx::with_dir(
        xwin::PathBuf::from(".xwin-cache/driver-test"),
        xwin::util::ProgressTarget::Hidden,
        ureq::agent(),
        0,
    )
    .unwrap();

    let ctx = std::sync::Arc::new(ctx);
    let hidden = indicatif::ProgressBar::hidden();

    // The WDK is only published for recent kits, so unlike the other tests we
    // always use a manifest new enough to have a matching SDK
    let manifest = xwin::manifest::get_manifest(&ctx, 17, "release", hidden.clone()).unwrap();
    let pkg_manifest =
        xwin::manifest::get_package_manifest(&ctx, &manifest, hidden.clone()).unwrap();

    let mut pruned = xwin::prune_pkg_list(
        &pkg_manifest,
        xwin::Arch::X86_64 as u32,
        xwin::Variant::Desktop as u32,
        false,
        false,
        None,
        None,
    )
    .unwrap();

    let wdk =
        xwin::nuget::get_wdk(&ctx, xwin::Arch::X86_64 as u32, None, &pruned.sdk_version).unwrap();

    pruned.payloads.extend(wdk.payloads);

    let output_dir = ctx.work_dir.join("splat");

    let op = xwin::Ops::Splat(xwin::SplatConfig {
        include_debug_libs: false,
        include_debug_symbols: false,
        enable_symlinks: true,
        preserve_ms_arch_notation: false,
        use_winsysroot_style: false,
        map: None,
        copy: true,
        output: output_dir.clone(),
    });

    ctx.clone()
        .execute(
            pkg_manifest.packages.clone(),
            pruned
                .payloads
                .into_iter()
                .map(|payload| xwin::WorkItem {
                    progress: hidden.clone(),
                    payload: std::sync::Arc::new(payload),
                })
                .collect(),
            pruned.crt_version,
            pruned.sdk_version,
            pruned.vcr_version,
            xwin::WdfVersions {
                kmdf: Some(KMDF_VERSION.to_owned()),
                umdf: None,
            },
            xwin::Arch::X86_64 as u32,
            xwin::Variant::Desktop as u32,
            op,
        )
        .unwrap();

    xwin::util::canonicalize(&output_dir).unwrap()
}

#[test]
fn verify_driver_compiles() {
    let od = splat_root();

    // Every one of these directories must exist for a driver build to work at all
    for expected in [
        "wdk/include/km/wdm.h",
        "wdk/include/km/ntddk.h",
        "wdk/include/shared",
        "wdk/include/um",
        &format!("wdk/include/wdf/kmdf/{KMDF_VERSION}/wdf.h"),
        "wdk/lib/km/x86_64/ntoskrnl.lib",
        "wdk/lib/km/x86_64/hal.lib",
        &format!("wdk/lib/wdf/kmdf/x86_64/{KMDF_VERSION}/wdfldr.lib"),
    ] {
        assert!(od.join(expected).exists(), "{expected} was not splatted");
    }

    // The WDK's msbuild props never link these by the casing they have on disk,
    // so if these are missing the casing fixups have regressed
    for canonical in [
        "wdk/lib/km/x86_64/BufferOverflowFastFailK.lib",
        &format!("wdk/lib/wdf/kmdf/x86_64/{KMDF_VERSION}/WdfLdr.lib"),
        &format!("wdk/lib/wdf/kmdf/x86_64/{KMDF_VERSION}/WdfDriverEntry.lib"),
    ] {
        assert!(
            od.join(canonical).exists(),
            "{canonical} symlink was not created"
        );
    }

    // None of the msbuild plumbing or windows executables should have survived
    for unwanted in [
        "wdk/bin",
        "wdk/build",
        "wdk/tools",
        "wdk/include/wdf/kmdf/1.15",
    ] {
        assert!(
            !od.join(unwanted).exists(),
            "{unwanted} should not have been splatted"
        );
    }

    let build_dir = od.parent().unwrap().join("driver-build");
    if build_dir.exists() {
        std::fs::remove_dir_all(&build_dir).unwrap();
    }
    std::fs::create_dir_all(&build_dir).unwrap();

    let obj = build_dir.join("driver.obj");

    // These are normally set by the WDK's msbuild props rather than the compiler
    let mut cc = Command::new("clang-cl");
    cc.args([
        "--target=x86_64-pc-windows-msvc",
        "/c",
        "/kernel",
        "/GS-",
        "/WX",
        "-D_AMD64_",
        "-DAMD64",
        "-D_WIN64",
        "-D_KERNEL_MODE",
        "-DNTDDI_VERSION=0x0A000010",
        "-D_WIN32_WINNT=0x0A00",
        "-DWINVER=0x0A00",
    ]);

    for include in [
        "crt/include".to_owned(),
        "sdk/include/ucrt".to_owned(),
        "sdk/include/shared".to_owned(),
        "sdk/include/um".to_owned(),
        "wdk/include/km".to_owned(),
        "wdk/include/shared".to_owned(),
        format!("wdk/include/wdf/kmdf/{KMDF_VERSION}"),
    ] {
        cc.arg("/imsvc").arg(od.join(include));
    }

    cc.arg(format!("/Fo{obj}"))
        .arg("tests/xwin-driver-test/driver.c");

    assert!(
        cc.status().unwrap().success(),
        "failed to compile the driver"
    );

    let sys = build_dir.join("xwin-driver-test.sys");

    // Note the libraries are deliberately named with the casing that Microsoft's
    // own props use, which is neither the casing on disk nor an all upper or
    // lower case version of it
    let mut link = Command::new("lld-link");
    link.args([
        "/NOLOGO",
        "/SUBSYSTEM:NATIVE",
        "/DRIVER",
        "/ENTRY:FxDriverEntry",
        "/NODEFAULTLIB",
    ])
    .arg(format!("/OUT:{sys}"))
    .arg(format!("/LIBPATH:{od}/wdk/lib/km/x86_64"))
    .arg(format!(
        "/LIBPATH:{od}/wdk/lib/wdf/kmdf/x86_64/{KMDF_VERSION}"
    ))
    .arg(obj)
    .args([
        "ntoskrnl.lib",
        "hal.lib",
        "wmilib.lib",
        "BufferOverflowFastFailK.lib",
        "WdfLdr.lib",
        "WdfDriverEntry.lib",
    ]);

    assert!(
        link.status().unwrap().success(),
        "failed to link the driver"
    );

    // A native subsystem PE is the actual deliverable, so check we produced one
    let pe = std::fs::read(&sys).unwrap();
    assert_eq!(&pe[..2], b"MZ", "{sys} is not a PE image");
}
