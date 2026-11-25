//! Temporary: measures copying a directory, to decide on the number of threads (#90).

use super::*;
use std::process::Command;
use std::time::{Duration, Instant};

const FILES: usize = 40_000;
const RUNS: usize = 5;

/// `copy_tree` as it was before: one file after another, on one thread.
fn serial(source: &Path, dest: &Path, reflink: bool, cloned: &mut Option<bool>) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            copy_link(&from, &to)?;
        } else if file_type.is_dir() {
            fs::create_dir(&to)?;
            serial(&from, &to, reflink, cloned)?;
        } else if file_type.is_file() {
            let this = copy_file(&from, &to, reflink, false)?;
            *cloned = Some(cloned.unwrap_or(true) && this.cloned);
        }
    }
    copy_dir_mode(source, dest)
}

/// Small files mostly, 2 KB in the middle and some 10 KB on average, 10 to a directory.
fn make_tree(root: &Path) -> u64 {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let noise: Vec<u8> = (0..1_000_000).map(|_| (random() >> 32) as u8).collect();
    let mut total = 0;
    for i in 0..FILES {
        let dir = root.join(format!("package-{:03}/lib/part-{}", i / 100, i / 10 % 10));
        if i % 10 == 0 {
            fs::create_dir_all(&dir).unwrap();
        }
        let (low, high) = match random() % 100 {
            0..60 => (200, 3_000),
            60..90 => (3_000, 15_000),
            90..99 => (15_000, 80_000),
            _ => (80_000, 500_000),
        };
        let size = (low + random() % (high - low)) as usize;
        let start = (random() % 500_000) as usize;
        fs::write(dir.join(format!("file-{i}.js")), &noise[start..][..size]).unwrap();
        total += size as u64;
    }
    total
}

/// Writes everything out and, if `cold`, forgets what is cached (Linux, passwordless `sudo`).
fn settle(cold: bool) {
    if cfg!(unix) {
        assert!(Command::new("sync").status().unwrap().success());
    } else {
        std::thread::sleep(Duration::from_secs(2));
    }
    if cold {
        let drop = ["sh", "-c", "echo 3 > /proc/sys/vm/drop_caches"];
        assert!(Command::new("sudo").args(drop).status().unwrap().success());
    }
}

#[test]
#[ignore = "a measurement"]
fn measure_copying() {
    let base = std::env::var_os("BP_BENCH_DIR").map(PathBuf::from);
    let base = base.unwrap_or_else(std::env::temp_dir);
    let cold_too = std::env::var_os("BP_BENCH_COLD").is_some_and(|v| !v.is_empty());
    let tmp = tempfile::tempdir_in(base).unwrap();
    let source = tmp.path().join("source");
    let total = make_tree(&source);
    println!(
        "{FILES} files, {} MB, {} CPUs",
        total / 1_000_000,
        thread::available_parallelism().unwrap()
    );

    // 0 is the serial copy as it was.
    let variants = [0, 1, 2, 4, 8, 16];
    for cold in [false, true] {
        if cold && !cold_too {
            continue;
        }
        let mut times = vec![Vec::new(); variants.len()];
        // Run 0 is not counted: it warms up.
        for run in 0..=RUNS {
            // Another order in every run, so that none always profits from its predecessor.
            for i in (0..variants.len()).map(|i| (i + run) % variants.len()) {
                let dest = tmp.path().join("dest");
                settle(cold);
                let started = Instant::now();
                fs::create_dir(&dest).unwrap();
                match variants[i] {
                    0 => serial(&source, &dest, false, &mut None).unwrap(),
                    n => assert_eq!(copy_tree(&source, &dest, false, n).unwrap(), Some(false)),
                }
                let took = started.elapsed();
                fs::remove_dir_all(&dest).unwrap();
                if run > 0 {
                    times[i].push(took.as_secs_f64());
                }
            }
        }
        println!("== {}", if cold { "cold" } else { "warm" });
        for (n, mut times) in variants.iter().zip(times) {
            times.sort_by(f64::total_cmp);
            let name = match n {
                0 => "before".to_owned(),
                n => format!("{n} threads"),
            };
            println!(
                "{name:<10} median {:6.2}s  min {:6.2}s  max {:6.2}s",
                times[RUNS / 2],
                times[0],
                times[RUNS - 1]
            );
        }
    }
}
