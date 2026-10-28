//! Speech model files: NVIDIA Parakeet TDT 0.6B v3, int8 ONNX export
//! (istupakov/parakeet-tdt-0.6b-v3-onnx, CC-BY-4.0). Pinned to a commit and
//! checked by size and SHA-256. Downloads resume after a dropped connection.

use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

const REPO: &str = "istupakov/parakeet-tdt-0.6b-v3-onnx";
const REVISION: &str = "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce";

struct ModelFile {
    name: &'static str,
    size: u64,
    sha256: &'static str,
}

const FILES: &[ModelFile] = &[
    ModelFile {
        name: "encoder-model.int8.onnx",
        size: 652_183_999,
        sha256: "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
    },
    ModelFile {
        name: "decoder_joint-model.int8.onnx",
        size: 18_202_004,
        sha256: "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
    },
    ModelFile {
        name: "nemo128.onnx",
        size: 139_764,
        sha256: "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
    },
    ModelFile {
        name: "vocab.txt",
        size: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    },
];

pub fn total_bytes() -> u64 {
    FILES.iter().map(|f| f.size).sum()
}

pub fn dir() -> PathBuf {
    crate::settings::data_dir().join("models").join("parakeet-tdt-0.6b-v3-int8")
}

/// Cheap check (sizes only); hashes are verified once, at download time.
pub fn is_installed() -> bool {
    FILES.iter().all(|f| fs::metadata(dir().join(f.name)).map(|m| m.len() == f.size).unwrap_or(false))
}

/// Downloads whatever is missing. `progress(done, total)` is called as bytes
/// arrive.
pub fn download(mut progress: impl FnMut(u64, u64)) -> Result<(), String> {
    let dir = dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let total = total_bytes();
    let mut done = 0u64;
    for file in FILES {
        let path = dir.join(file.name);
        if fs::metadata(&path).map(|m| m.len() == file.size).unwrap_or(false) {
            done += file.size;
            progress(done, total);
            continue;
        }
        let part = dir.join(format!("{}.part", file.name));
        download_file(file, &part, |n| progress(done + n, total))?;
        fs::rename(&part, &path).map_err(|e| e.to_string())?;
        done += file.size;
        log::info!("downloaded {}", file.name);
    }
    Ok(())
}

fn download_file(file: &ModelFile, part: &PathBuf, mut progress: impl FnMut(u64)) -> Result<(), String> {
    let mut hasher = Sha256::new();
    let mut have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if have > file.size {
        have = 0;
    }
    // Re-hash what an earlier attempt already saved.
    if have > 0 {
        let mut f = File::open(part).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }

    let url = format!("https://huggingface.co/{REPO}/resolve/{REVISION}/{}", file.name);
    let mut req = ureq::get(&url);
    if have > 0 {
        req = req.header("Range", &format!("bytes={have}-"));
    }
    let resp = req.call().map_err(|e| format!("{}: {e}", file.name))?;
    let resumed = resp.status().as_u16() == 206;
    if have > 0 && !resumed {
        // Server ignored the range; start over.
        have = 0;
        hasher = Sha256::new();
    }
    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(part)
        .map_err(|e| e.to_string())?;

    let mut reader = resp.into_body().into_reader();
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = have;
    progress(got);
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("{}: {e}", file.name))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        hasher.update(&buf[..n]);
        got += n as u64;
        progress(got);
    }
    out.flush().map_err(|e| e.to_string())?;
    drop(out);

    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if got != file.size || hex != file.sha256 {
        let _ = fs::remove_file(part);
        return Err(format!("{} failed verification, try again", file.name));
    }
    Ok(())
}
