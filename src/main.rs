use anyhow::{Context, Result};
use clap::Parser;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "icon_gen", about = "Generate favicon icon sets from a single SVG")]
struct Args {
    #[arg(short, long, help = "Input SVG file")]
    input: PathBuf,

    #[arg(long, help = "Input Markdown template used for AUTO_FILE_LIST_START/END replacement")]
    input_markdown: Option<PathBuf>,

    #[arg(short, long, help = "Output directory")]
    output: PathBuf,

    #[arg(short, long, help = "Also write a README.md with a file-list in the output directory")]
    gen_markdown: bool,

    #[arg(long, help = "Output markdown path (default: <output>/README.md)")]
    output_markdown: Option<PathBuf>,
}

/// PNG sizes to generate for each favicon set.
const PNG_SIZES: &[(u32, &str)] = &[
    (16, "favicon-16x16.png"),
    (32, "favicon-32x32.png"),
    (180, "apple-touch-icon.png"),
    (192, "android-chrome-192x192.png"),
    (512, "android-chrome-512x512.png"),
];

/// Sizes bundled into favicon.ico.
const ICO_SIZES: &[u32] = &[16, 32, 48];

const WEBMANIFEST: &str = r##"{
    "name": "",
    "short_name": "",
    "icons": [
        {
            "src": "/android-chrome-192x192.png",
            "sizes": "192x192",
            "type": "image/png"
        },
        {
            "src": "/android-chrome-512x512.png",
            "sizes": "512x512",
            "type": "image/png"
        }
    ],
    "theme_color": "#ffffff",
    "background_color": "#ffffff",
    "display": "standalone"
}
"##;

// ---------------------------------------------------------------------------
// SVG colour-variant helpers
// ---------------------------------------------------------------------------

/// Injects an SVG `feColorMatrix` invert filter and wraps all child content in
/// a `<g filter="…">` so the rendered result has inverted colours.
fn apply_invert_filter(svg: &str) -> String {
    let filter_def = concat!(
        r#"<defs>"#,
        r#"<filter id="__icg_inv__" color-interpolation-filters="sRGB">"#,
        r#"<feColorMatrix type="matrix" "#,
        r#"values="-1 0 0 0 1  0 -1 0 0 1  0 0 -1 0 1  0 0 0 1 0"/>"#,
        r#"</filter>"#,
        r#"</defs>"#,
        r#"<g filter="url(#__icg_inv__)">"#,
    );

    let Some(svg_start) = svg.find("<svg") else {
        return svg.to_string();
    };
    let Some(tag_end_rel) = svg[svg_start..].find('>') else {
        return svg.to_string();
    };
    let insert_pos = svg_start + tag_end_rel + 1;
    let before = &svg[..insert_pos];
    let after = &svg[insert_pos..];

    let Some(close_pos) = after.rfind("</svg>") else {
        return svg.to_string();
    };
    let content = &after[..close_pos];
    let tail = &after[close_pos..];

    format!("{before}{filter_def}{content}</g>{tail}")
}

fn extract_attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=");
    let start = tag.find(&needle)? + needle.len();
    let quote = tag.get(start..=start)?;
    if quote != "\"" && quote != "'" {
        return None;
    }
    let rest = &tag[start + 1..];
    let end_rel = rest.find(quote)?;
    Some(rest[..end_rel].trim().to_ascii_lowercase())
}

fn normalize_fill(tag: &str) -> Option<String> {
    if let Some(fill) = extract_attr_value(tag, "fill") {
        return Some(fill);
    }
    let style = extract_attr_value(tag, "style")?;
    for part in style.split(';') {
        let mut kv = part.splitn(2, ':');
        let Some(k) = kv.next() else { continue };
        let Some(v) = kv.next() else { continue };
        if k.trim().eq_ignore_ascii_case("fill") {
            return Some(v.trim().to_ascii_lowercase());
        }
    }
    None
}

fn is_zero_or_default(v: Option<String>) -> bool {
    match v {
        None => true,
        Some(x) => {
            let t = x.trim();
            t == "0" || t == "0.0" || t == "0%"
        }
    }
}

fn is_full_100(v: Option<String>) -> bool {
    match v {
        Some(x) => x.trim() == "100%",
        None => false,
    }
}

fn is_bw_fill(fill: &str) -> bool {
    let f = fill.trim().replace(' ', "");
    matches!(
        f.as_str(),
        "#fff"
            | "#ffffff"
            | "white"
            | "rgb(255,255,255)"
            | "rgba(255,255,255,1)"
            | "#000"
            | "#000000"
            | "black"
            | "rgb(0,0,0)"
            | "rgba(0,0,0,1)"
    )
}

/// Removes simple full-canvas black/white background rects like:
/// `<rect x="0" y="0" width="100%" height="100%" fill="#fff"/>`
fn strip_bw_background_rect(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut cursor = 0usize;

    while let Some(rel) = svg[cursor..].find("<rect") {
        let start = cursor + rel;
        out.push_str(&svg[cursor..start]);

        let Some(end_rel) = svg[start..].find('>') else {
            out.push_str(&svg[start..]);
            return out;
        };
        let tag_end = start + end_rel;
        let tag = &svg[start..=tag_end];

        let fill = normalize_fill(tag);
        let is_bg = fill.as_deref().is_some_and(is_bw_fill)
            && is_zero_or_default(extract_attr_value(tag, "x"))
            && is_zero_or_default(extract_attr_value(tag, "y"))
            && is_full_100(extract_attr_value(tag, "width"))
            && is_full_100(extract_attr_value(tag, "height"));

        if is_bg {
            if tag.trim_end().ends_with("/>") {
                cursor = tag_end + 1;
                continue;
            }
            if let Some(close_rel) = svg[tag_end + 1..].find("</rect>") {
                cursor = tag_end + 1 + close_rel + "</rect>".len();
                continue;
            }
        }

        out.push_str(tag);
        cursor = tag_end + 1;
    }

    out.push_str(&svg[cursor..]);
    out
}

/// Adds a full-canvas background rectangle at the start of SVG content.
fn add_background_rect(svg: &str, fill: &str) -> String {
    let Some(svg_start) = svg.find("<svg") else {
        return svg.to_string();
    };
    let Some(tag_end_rel) = svg[svg_start..].find('>') else {
        return svg.to_string();
    };
    let insert_pos = svg_start + tag_end_rel + 1;
    let before = &svg[..insert_pos];
    let after = &svg[insert_pos..];
    format!(
        "{before}<rect x=\"0\" y=\"0\" width=\"100%\" height=\"100%\" fill=\"{fill}\"/>{after}"
    )
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_png(svg_data: &str, size: u32) -> Result<Vec<u8>> {
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_str(svg_data, &opt).context("Failed to parse SVG")?;

    let mut pixmap =
        tiny_skia::Pixmap::new(size, size).context("Failed to allocate pixmap")?;

    let scale_x = size as f32 / tree.size().width();
    let scale_y = size as f32 / tree.size().height();

    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale_x, scale_y),
        &mut pixmap.as_mut(),
    );

    pixmap.encode_png().context("Failed to encode PNG")
}

fn generate_ico(svg_data: &str, path: &Path) -> Result<()> {
    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);

    for &size in ICO_SIZES {
        let opt = usvg::Options::default();
        let tree =
            usvg::Tree::from_str(svg_data, &opt).context("Failed to parse SVG for ICO")?;

        let mut pixmap = tiny_skia::Pixmap::new(size, size)
            .context("Failed to allocate pixmap for ICO")?;

        let scale_x = size as f32 / tree.size().width();
        let scale_y = size as f32 / tree.size().height();

        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale_x, scale_y),
            &mut pixmap.as_mut(),
        );

        let image = ico::IconImage::from_rgba_data(size, size, pixmap.data().to_vec());
        icon_dir.add_entry(ico::IconDirEntry::encode(&image).context("ICO encode error")?);
    }

    let file = fs::File::create(path)
        .with_context(|| format!("Cannot create {}", path.display()))?;
    icon_dir.write(file).context("Failed to write ICO")?;
    Ok(())
}

fn generate_favicon_set(svg_data: &str, dir: &Path, label: &str) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("Cannot create directory {}", dir.display()))?;

    for &(size, filename) in PNG_SIZES {
        let png = render_png(svg_data, size)
            .with_context(|| format!("Render failed for {filename} ({size}x{size})"))?;
        fs::write(dir.join(filename), &png)
            .with_context(|| format!("Write failed for {filename}"))?;
        println!("  {label}/{filename}");
    }

    generate_ico(svg_data, &dir.join("favicon.ico"))
        .with_context(|| format!("ICO generation failed in {label}"))?;
    println!("  {label}/favicon.ico");

    fs::write(dir.join("site.webmanifest"), WEBMANIFEST)?;
    println!("  {label}/site.webmanifest");

    Ok(())
}

// ---------------------------------------------------------------------------
// Markdown generation  (mirrors the Python script's logic)
// ---------------------------------------------------------------------------

/// Convert byte count to a human-readable string (B / K / M / G / T).
/// Matches the Python `human_readable_size` format exactly.
fn human_readable_size(bytes: u64) -> String {
    if bytes == 0 {
        return "0".to_string();
    }
    let units = ['B', 'K', 'M', 'G', 'T'];
    let mut size = bytes as f64;
    for &unit in &units {
        if size < 1024.0 {
            return if unit == 'B' {
                format!("{:.0}{unit}", size)
            } else {
                // One decimal place, strip trailing zeros and dot (like Python rstrip)
                let s = format!("{:.1}", size);
                let s = s.trim_end_matches('0').trim_end_matches('.');
                format!("{s}{unit}")
            };
        }
        size /= 1024.0;
    }
    format!("{size:.1}T")
}

/// Returns a built-in description for well-known favicon filenames, or `""`.
fn description_for(name: &str) -> &'static str {
    match name {
        "favicon-16x16.png"       => "16x16",
        "favicon-32x32.png"       => "32x32",
        "apple-touch-icon.png"    => "180x180",
        "android-chrome-192x192.png" => "192x192",
        "android-chrome-512x512.png" => "512x512",
        "favicon.ico"             => "48x48",
        "site.webmanifest"        => "Webmanifest config file",
        "icon.svg"                => "Source",
        "icon-dark.svg"           => "Dark mode source",
        "icon-light.svg"          => "Light mode source",
        _                         => "",
    }
}

/// Build the markdown file-list for `dir`.
///
/// Directories are rendered as bold headings with indented children;
/// files as links with size and an optional description.  Matches the
/// format produced by the Python script.
fn build_markdown_list(dir: &Path) -> Result<String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("Cannot read dir {}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();

    // Sort: directories first, then files; each group alphabetically.
    entries.sort_by(|a, b| {
        let a_is_dir = a.path().is_dir();
        let b_is_dir = b.path().is_dir();
        match (a_is_dir, b_is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.file_name().cmp(&b.file_name()),
        }
    });

    let mut lines: Vec<String> = Vec::new();

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if path.is_dir() {
            lines.push(format!("- **{name}/**"));

            // One level of children (matches the Python script's fixed-depth handling).
            let mut children: Vec<_> = fs::read_dir(&path)
                .with_context(|| format!("Cannot read dir {}", path.display()))?
                .filter_map(|e| e.ok())
                .collect();
            children.sort_by_key(|e| e.file_name());

            for child in children {
                let cpath = child.path();
                let cname = child.file_name();
                let cname = cname.to_string_lossy();
                let rel = format!("{name}/{cname}");

                if cpath.is_dir() {
                    lines.push(format!("  - [**{cname}/**](./{rel}/)"));
                } else {
                    let size = fs::metadata(&cpath)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    let size_str = human_readable_size(size);
                    let desc = description_for(&cname);
                    let desc_part = if desc.is_empty() {
                        String::new()
                    } else {
                        format!(" - **{desc}**")
                    };
                    lines.push(format!(
                        "  - [{cname}](./{rel}) *({size_str})*{desc_part}"
                    ));
                }
            }
        } else {
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let size_str = human_readable_size(size);
            let desc = description_for(&name);
            let desc_part = if desc.is_empty() {
                String::new()
            } else {
                format!(" - **{desc}**")
            };
            lines.push(format!("- [{name}](./{name}) *({size_str})*{desc_part}"));
        }
    }

    Ok(lines.join("\n"))
}

/// Write `README.md` in `out_dir` containing the file list.
///
/// When `template_md` is provided, only the content between
/// `AUTO_FILE_LIST_START/END` markers is replaced.
fn write_markdown(out_dir: &Path, template_md: Option<&Path>, output_md: Option<&Path>) -> Result<()> {
    let list = build_markdown_list(out_dir)?;

    let content = if let Some(template_path) = template_md {
        let template = fs::read_to_string(template_path)
            .with_context(|| format!("Cannot read markdown template: {}", template_path.display()))?;

        let start_marker = "<!-- AUTO_FILE_LIST_START -->";
        let end_marker = "<!-- AUTO_FILE_LIST_END -->";

        let start_idx = template
            .find(start_marker)
            .ok_or_else(|| anyhow::anyhow!("Missing AUTO_FILE_LIST_START marker in {}", template_path.display()))?;
        let after_start = start_idx + start_marker.len();
        let end_rel = template[after_start..]
            .find(end_marker)
            .ok_or_else(|| anyhow::anyhow!("Missing AUTO_FILE_LIST_END marker in {}", template_path.display()))?;
        let end_idx = after_start + end_rel;

        format!(
            "{}{}\n\n{}\n\n{}{}",
            &template[..start_idx],
            start_marker,
            list,
            end_marker,
            &template[end_idx + end_marker.len()..]
        )
    } else {
        format!(
            "# Icons\n\n<!-- AUTO_FILE_LIST_START -->\n\n{list}\n\n<!-- AUTO_FILE_LIST_END -->\n"
        )
    };

    let readme = output_md
        .map(Path::to_path_buf)
        .unwrap_or_else(|| out_dir.join("README.md"));
    if let Some(parent) = readme.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Cannot create markdown output directory: {}", parent.display())
            })?;
        }
    }
    fs::write(&readme, content)?;
    println!("Wrote {}", readme.display());
    Ok(())
}
// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse();

    let svg_content = fs::read_to_string(&args.input)
        .with_context(|| format!("Cannot read input: {}", args.input.display()))?;

    let out = &args.output;
    fs::create_dir_all(out)
        .with_context(|| format!("Cannot create output dir: {}", out.display()))?;

    // Strip common solid black/white full-canvas backgrounds from color-scheme
    // variants so dark/light outputs keep transparent backgrounds.
    let transparent_base_svg = strip_bw_background_rect(&svg_content);

    // dark  → original icon on black background (for transparent-source inputs)
    // light → original icon on white background (for transparent-source inputs)
    let dark_svg = add_background_rect(&transparent_base_svg, "#000000");
    let light_svg = add_background_rect(&transparent_base_svg, "#ffffff");

    fs::write(out.join("icon.svg"), &svg_content)?;
    fs::write(out.join("icon-dark.svg"), &dark_svg)?;
    fs::write(out.join("icon-light.svg"), &light_svg)?;
    println!("Saved SVG variants.");

    println!("\nGenerating favicon/ ...");
    generate_favicon_set(&svg_content, &out.join("favicon"), "favicon")?;

    println!("\nGenerating favicon-dark/ ...");
    generate_favicon_set(&dark_svg, &out.join("favicon-dark"), "favicon-dark")?;

    println!("\nGenerating favicon-light/ ...");
    generate_favicon_set(&light_svg, &out.join("favicon-light"), "favicon-light")?;

    if args.gen_markdown || args.input_markdown.is_some() || args.output_markdown.is_some() {
        println!("\nGenerating README.md ...");
        write_markdown(
            out,
            args.input_markdown.as_deref(),
            args.output_markdown.as_deref(),
        )?;
    }

    println!("\nDone -> {}", out.display());
    Ok(())
}
