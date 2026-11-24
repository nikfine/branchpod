//! Holders of files as the Restart Manager lists them.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::{fs, iter, ptr};

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
    RmStartSession,
};
use windows_sys::core::PCWSTR;

use super::Holder;

/// Files of a large tree registered with the Restart Manager. Enough to find an editor or a
/// dev server; the walk stops there rather than enumerate a whole dependency directory.
const MAX_FILES: usize = 4096;

pub fn holders(path: &Path) -> Vec<Holder> {
    query(&files_under(path, MAX_FILES)).unwrap_or_default()
}

/// `path` and, for a directory, the files below it, without following links.
fn files_under(path: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        if files.len() >= limit {
            break;
        }
        match fs::symlink_metadata(&p) {
            Ok(meta) if meta.is_dir() => {
                if let Ok(entries) = fs::read_dir(&p) {
                    stack.extend(entries.flatten().map(|e| e.path()));
                }
            }
            Ok(_) => files.push(p),
            Err(_) => {}
        }
    }
    files
}

struct Session(u32);

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful `RmStartSession` and is ended once.
        unsafe { RmEndSession(self.0) };
    }
}

fn query(files: &[PathBuf]) -> Option<Vec<Holder>> {
    if files.is_empty() {
        return None;
    }
    let mut handle = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    // SAFETY: both pointers are valid for the duration of the call; `key` has room for
    // CCH_RM_SESSION_KEY characters plus the terminator, as the API requires.
    if unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) } != ERROR_SUCCESS {
        return None;
    }
    let session = Session(handle);

    let wide: Vec<Vec<u16>> = files
        .iter()
        .map(|p| p.as_os_str().encode_wide().chain(iter::once(0)).collect())
        .collect();
    let names: Vec<PCWSTR> = wide.iter().map(|w| w.as_ptr()).collect();
    // SAFETY: `names` holds `names.len()` pointers to NUL-terminated strings that outlive the
    // call; no applications or services are passed.
    let registered = unsafe {
        RmRegisterResources(
            session.0,
            names.len() as u32,
            names.as_ptr(),
            0,
            ptr::null(),
            0,
            ptr::null(),
        )
    };
    if registered != ERROR_SUCCESS {
        return None;
    }

    let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
    // The list can grow between the size query and the fetch; give up after a few rounds.
    for _ in 0..4 {
        let mut needed = 0u32;
        let mut count = infos.len() as u32;
        let mut reasons = 0u32;
        // SAFETY: `infos` has room for `count` entries; the API writes at most that many.
        let rc = unsafe {
            RmGetList(
                session.0,
                &mut needed,
                &mut count,
                if infos.is_empty() {
                    ptr::null_mut()
                } else {
                    infos.as_mut_ptr()
                },
                &mut reasons,
            )
        };
        if rc == ERROR_SUCCESS {
            infos.truncate(count as usize);
            return Some(holders_of(&infos));
        }
        if rc != ERROR_MORE_DATA {
            return None;
        }
        infos.resize(needed as usize, RM_PROCESS_INFO::default());
    }
    None
}

fn holders_of(infos: &[RM_PROCESS_INFO]) -> Vec<Holder> {
    let mut holders: Vec<Holder> = Vec::new();
    for info in infos {
        let pid = info.Process.dwProcessId;
        if holders.iter().any(|h| h.pid == pid) {
            continue;
        }
        let name = &info.strAppName;
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        holders.push(Holder {
            pid,
            name: String::from_utf16_lossy(&name[..len]),
        });
    }
    holders
}
