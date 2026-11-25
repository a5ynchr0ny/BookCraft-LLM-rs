//! 合成示例 EPUB（法语短篇，带行内标签与对白），用于 demo 与测试。

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const CONTAINER: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"##;

const OPF: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">bookcraft-sample</dc:identifier>
    <dc:title>L'Éducation sentimentale — Extrait</dc:title>
    <dc:creator>Gustave Flaubert</dc:creator>
    <dc:language>fr</dc:language>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="c1" href="chap_01.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="chap_02.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="c1"/>
    <itemref idref="c2"/>
  </spine>
</package>
"##;

const NCX: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head><meta name="dtb:uid" content="bookcraft-sample"/></head>
  <docTitle><text>Extrait</text></docTitle>
  <navMap>
    <navPoint id="p1" playOrder="1">
      <navLabel><text>Chapitre premier</text></navLabel>
      <content src="chap_01.xhtml"/>
    </navPoint>
    <navPoint id="p2" playOrder="2">
      <navLabel><text>Chapitre II</text></navLabel>
      <content src="chap_02.xhtml"/>
    </navPoint>
  </navMap>
</ncx>
"##;

const CHAP_ONE: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="fr" xml:lang="fr">
<head><title>Chapitre premier</title></head>
<body>
<h1>Chapitre premier</h1>
<p>Le 15 septembre 1840, vers six heures du matin, le bateau se trouvait près de
l'île Saint-Louis, et le jeune homme regardait les fenêtres du quai.</p>
<p>Il dit <i>bonjour</i> à M<sup>me</sup> Arnoux.<a class="note" href="#n1">[1]</a>
Elle ne répondit pas tout de suite.</p>
<blockquote><p>« Je vous aime », dit-elle enfin, avec un sourire triste.</p></blockquote>
<p>Le ciel était gris, l'eau <b>froide</b> et la ville semblait dormir.<br/>
Personne ne bougeait.</p>
<p>— Vous partez déjà ? demanda Frédéric.</p>
</body>
</html>
"##;

const CHAP_TWO: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="fr" xml:lang="fr">
<head><title>Chapitre II</title></head>
<body>
<h2>Chapitre II</h2>
<p>Frédéric Moreau rentra chez lui. Moreau relut la lettre de Madame Arnoux,
puis il la posa sur la table et regarda longtemps la Seine.</p>
<p>Le lendemain, Deslauriers vint le chercher. « Tu es fou », lui dit-il.
« Nous ne sommes pas des enfants. »</p>
</body>
</html>
"##;

pub fn build_sample_epub(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(path).with_context(|| format!("创建 {} 失败", path.display()))?;
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;

    let entries = [
        ("META-INF/container.xml", CONTAINER),
        ("OEBPS/content.opf", OPF),
        ("OEBPS/toc.ncx", NCX),
        ("OEBPS/chap_01.xhtml", CHAP_ONE),
        ("OEBPS/chap_02.xhtml", CHAP_TWO),
    ];
    for (name, content) in entries {
        zip.start_file(name, deflated)?;
        zip.write_all(content.as_bytes())?;
    }
    zip.finish()?;
    Ok(path.to_path_buf())
}
