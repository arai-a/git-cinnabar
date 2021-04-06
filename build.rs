/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::Path;
use std::process::Command;

use count_write::CountWrite;
use git_version::git_version;
use itertools::Itertools;
use make_cmd::gnu_make;
use tar::{Builder, EntryType, Header};
use walkdir::WalkDir;

const GIT_VERSION: &str = git_version!(args = ["--always", "--abbrev=40"]);

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("Failed to get {}", name))
}

fn env_os(name: &str) -> OsString {
    std::env::var_os(name).unwrap_or_else(|| panic!("Failed to get {}", name))
}

fn prepare_make(make: &mut Command) -> &mut Command {
    let mut result = make.arg("-f").arg("../src/helper.mk");

    for chunk in &std::env::var("CINNABAR_MAKE_FLAGS")
        .unwrap_or_else(|_| "".into())
        .split('\'')
        .chunks(2)
    {
        let chunk: Vec<_> = chunk.collect();
        if chunk.len() == 2 {
            let name = chunk[0].trim_start().trim_end_matches('=');
            let value = chunk[1];
            result = result.arg(&format!("{}={}", name, value));
        }
    }
    result.env_remove("PROFILE")
}

#[rustversion::all(nightly, before(2020-11-23))]
fn feature_bool_to_option() {
    println!("cargo:rustc-cfg=feature_bool_to_option");
}
#[rustversion::not(all(nightly, before(2020-11-23)))]
fn feature_bool_to_option() {}

#[rustversion::all(nightly, before(2020-12-27))]
fn feature_min_const_generics() {
    println!("cargo:rustc-cfg=feature_min_const_generics");
}
#[rustversion::not(all(nightly, before(2020-12-27)))]
fn feature_min_const_generics() {}

#[rustversion::all(nightly, before(2021-01-07))]
fn feature_slice_strip() {
    println!("cargo:rustc-cfg=feature_slice_strip");
}
#[rustversion::not(all(nightly, before(2021-01-07)))]
fn feature_slice_strip() {}

fn main() {
    let target_arch = env("CARGO_CFG_TARGET_ARCH");
    let target_os = env("CARGO_CFG_TARGET_OS");
    let target_env = env("CARGO_CFG_TARGET_ENV");
    let target_endian = env("CARGO_CFG_TARGET_ENDIAN");
    let target_pointer_width = env("CARGO_CFG_TARGET_POINTER_WIDTH");
    if target_os == "windows" && target_env != "gnu" {
        panic!(
            "Compilation for {}-{} is not supported",
            target_os, target_env
        );
    }
    if std::env::var("CINNABAR_CROSS_COMPILE_I_KNOW_WHAT_I_M_DOING").is_err()
        && target_arch != target::arch()
        || target_os != target::os()
        || target_env != target::env()
        || target_endian != target::endian()
        || target_pointer_width != target::pointer_width()
    {
        panic!("Cross-compilation is not supported");
    }

    let dir = env_os("CARGO_MANIFEST_DIR");
    let dir = Path::new(&dir);

    let git_core = dir.join("git-core");

    let mut make = gnu_make();
    let cmd = prepare_make(&mut make);
    cmd.arg("libcinnabar.a")
        .arg("V=1")
        .arg("HAVE_WPGMPTR=")
        .arg("USE_LIBPCRE1=")
        .arg("USE_LIBPCRE2=")
        .arg("FSMONITOR_DAEMON_BACKEND=");

    let cflags_var = format!("CFLAGS_{}", env("TARGET").replace("-", "_"));
    let cflags = [std::env::var(&cflags_var), std::env::var("TARGET_CFLAGS")]
        .iter()
        .filter_map(|v| v.as_ref().ok())
        .join(" ");
    if !cflags.is_empty() {
        cmd.arg(format!("CFLAGS+={}", cflags));
    }
    println!("cargo:rerun-if-env-changed={}", cflags_var);
    println!("cargo:rerun-if-env-changed=TARGET_CFLAGS");

    assert!(cmd
        .env("MAKEFLAGS", format!("-j {}", env("CARGO_MAKEFLAGS")))
        .current_dir(&git_core)
        .status()
        .expect("Failed to execute GNU make")
        .success());

    let mut make = gnu_make();
    let output = prepare_make(&mut make)
        .arg("--no-print-directory")
        .arg("linker-flags")
        .arg("USE_LIBPCRE1=")
        .arg("USE_LIBPCRE2=")
        .current_dir(&git_core)
        .output()
        .expect("Failed to execute GNU make");
    let output = String::from_utf8(output.stdout).unwrap();

    println!("cargo:rustc-link-lib=static=cinnabar");
    println!("cargo:rustc-link-search=native={}", git_core.display());

    if target_os == "windows" && target_env == "gnu" {
        println!("cargo:rustc-link-lib=ssp_nonshared");
        println!("cargo:rustc-link-lib=ssp");
    }

    for flag in output.split_whitespace() {
        if let Some(lib) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={}", lib);
        } else if let Some(libdir) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={}", libdir);
        }
    }

    for src in fs::read_dir(&dir).unwrap() {
        let path = src.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if (name.ends_with(".h")
            || name.ends_with(".c")
            || name.ends_with(".c.patch")
            || name.ends_with(".rs")
            || name.ends_with(".mk"))
            && !name.ends_with("patched.c")
        {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    println!("cargo:rerun-if-env-changed=CINNABAR_MAKE_FLAGS");

    let helper_hash = if GIT_VERSION.is_empty() {
        "unknown"
    } else {
        &GIT_VERSION[GIT_VERSION.len() - 40..]
    };
    println!("cargo:rustc-env=HELPER_HASH={}", helper_hash);
    feature_bool_to_option();
    feature_min_const_generics();
    feature_slice_strip();

    let dir = dir.join("cinnabar");
    let python_tar = Path::new(&env_os("OUT_DIR")).join("python.tar");
    let output = File::create(&python_tar).unwrap();
    let mut builder = Builder::new(CountWrite::from(output));
    let mut python_files = WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| {
            e.ok()
                .filter(|e| e.path().extension() == Some(OsStr::new("py")))
        })
        .collect::<Vec<_>>();
    python_files.sort_unstable_by(|a, b| a.path().cmp(b.path()));

    for entry in python_files {
        println!("cargo:rerun-if-changed={}", entry.path().display());
        let mut header = Header::new_gnu();
        header
            .set_path(entry.path().strip_prefix(&dir).unwrap())
            .unwrap();
        header.set_size(entry.metadata().unwrap().len());
        header.set_mode(0o644);
        header.set_entry_type(EntryType::Regular);
        header.set_cksum();
        builder
            .append(&header, File::open(entry.path()).unwrap())
            .unwrap();
    }
    let counter = builder.into_inner().unwrap();
    let size = counter.count();
    println!("cargo:rustc-env=PYTHON_TAR={}", python_tar.display());
    println!("cargo:rustc-env=PYTHON_TAR_SIZE={}", size);
}
