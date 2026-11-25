//! 生成用于 demo / 测试的合成 EPUB。

use std::path::Path;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/bookcraft-sample.epub".to_string());
    let path = bookcraft::sample::build_sample_epub(Path::new(&path))?;
    println!("{}", path.display());
    Ok(())
}
