//! A synthetic Rust/TypeScript/Python repository with a fixed seed, so every run of the
//! gates indexes the same files.

use std::path::Path;

/// The marker that [`generate`] puts in exactly one file (a selective search's target).
pub const NEEDLE: &str = "the_one_needle_marker";
/// The 500-line Rust file [`generate`] always writes.
pub const BIG_FILE: &str = "src/big.rs";

/// Write `files` source files under `root` (a third each Rust, TypeScript and Python),
/// each with `fns` functions that call names from a shared vocabulary, plus
/// [`BIG_FILE`]. The same arguments always give the same tree.
pub fn generate(root: &Path, files: usize, fns: usize) {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let vocab: Vec<String> = (0..2000)
        .map(|i| format!("w{:x}_{i}", next() % 100_000))
        .collect();
    let mut pick = move || vocab[(next() % vocab.len() as u64) as usize].clone();
    for i in 0..files {
        let dir = root.join(format!("pkg{}/mod{}", i % 100, i % 10));
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        let ext = match i % 3 {
            0 => {
                for _ in 0..fns {
                    let (a, b) = (pick(), pick());
                    body.push_str(&format!(
                        "pub fn {a}(x: u32) -> u32 {{\n    {b}(x) + 1\n}}\n"
                    ));
                }
                "rs"
            }
            1 => {
                for _ in 0..fns {
                    let (a, b) = (pick(), pick());
                    body.push_str(&format!(
                        "export function {a}(x: number): number {{\n  return {b}(x) + 1;\n}}\n"
                    ));
                }
                "ts"
            }
            _ => {
                for _ in 0..fns {
                    let (a, b) = (pick(), pick());
                    body.push_str(&format!("def {a}(x):\n    return {b}(x) + 1\n"));
                }
                "py"
            }
        };
        if i == files / 2 {
            body.push_str(&format!("// {NEEDLE}\n"));
        }
        std::fs::write(dir.join(format!("f{i}.{ext}")), body).unwrap();
    }
    let big = root.join(BIG_FILE);
    std::fs::create_dir_all(big.parent().unwrap()).unwrap();
    std::fs::write(&big, big_file(0)).unwrap();
}

/// The text of [`BIG_FILE`] (500 lines), varied by `n` so each rewrite is a real change.
pub fn big_file(n: usize) -> String {
    (0..125)
        .map(|i| {
            format!("pub fn big_{i}_{n}(x: u32) -> u32 {{\n    let y = x * {i};\n    y + {n}\n}}\n")
        })
        .collect()
}
