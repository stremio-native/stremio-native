use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

type TranslationMap = BTreeMap<String, String>;

fn write_if_changed(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    std::fs::write(path, contents)
}

fn read_json_translation(path: &Path) -> std::io::Result<TranslationMap> {
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn normalize_placeholders(s: &str) -> String {
    let res = s.replace("%s", "{}").replace("%d", "{}");
    let mut output = String::new();
    let mut chars = res.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' && chars.peek() == Some(&'{') {
            chars.next();
            output.push_str("{}");
            while let Some(next_c) = chars.next() {
                if next_c == '}' && chars.peek() == Some(&'}') {
                    chars.next();
                    break;
                }
            }
        } else if matches!(c, '$' | '#') && chars.peek() == Some(&'{') {
            chars.next();
            output.push_str("{}");
            for next_c in chars.by_ref() {
                if next_c == '}' {
                    break;
                }
            }
        } else {
            output.push(c);
        }
    }
    output
}

fn escape_po_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn normalize_build_version(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric()))
    .then(|| value.chars().take(12).collect())
}

fn build_version() -> String {
    ["STREMIO_BUILD_VERSION", "GITHUB_SHA", "CI_COMMIT_SHA"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .and_then(|value| normalize_build_version(&value))
        })
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .current_dir("..")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|value| normalize_build_version(&value))
        })
        .unwrap_or_else(|| "development".to_owned())
}

fn translation_paths(translations_src_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(translations_src_dir)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| !stem.starts_with("package"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn generate_po_files(translations_src_dir: &Path, output_dir: &Path) -> std::io::Result<()> {
    let en_map = read_json_translation(&translations_src_dir.join("en-US.json"))?;

    let mut english_counts = BTreeMap::new();
    for value in en_map.values() {
        let normalized = normalize_placeholders(value);
        *english_counts.entry(normalized).or_insert(0) += 1;
    }
    let duplicate_english = english_counts
        .into_iter()
        .filter_map(|(value, count)| (count > 1).then_some(value))
        .collect::<BTreeSet<_>>();

    if output_dir.exists() {
        std::fs::remove_dir_all(output_dir)?;
    }

    for path in translation_paths(translations_src_dir)? {
        let Some(locale) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let lang_map = read_json_translation(&path)?;
        let mut po_content = format!(
            "msgid \"\"\nmsgstr \"\"\n\"Language: {}\\n\"\n\"MIME-Version: 1.0\\n\"\n\"Content-Type: text/plain; charset=UTF-8\\n\"\n\"Content-Transfer-Encoding: 8bit\\n\"\n\n",
            escape_po_string(locale)
        );
        let mut written_plain_msgids = BTreeSet::new();

        for (key, english) in &en_map {
            let Some(translation) = lang_map.get(key) else {
                continue;
            };
            let english = normalize_placeholders(english);
            let translation = normalize_placeholders(translation);

            // A malformed upstream entry must not break Slint formatting at runtime.
            if english.matches("{}").count() != translation.matches("{}").count() {
                continue;
            }

            let escaped_key = escape_po_string(key);
            let escaped_english = escape_po_string(&english);
            let escaped_translation = escape_po_string(&translation);
            po_content.push_str(&format!(
                "msgctxt \"{escaped_key}\"\nmsgid \"{escaped_english}\"\nmsgstr \"{escaped_translation}\"\n\n"
            ));

            if !duplicate_english.contains(&english) && written_plain_msgids.insert(english) {
                po_content.push_str(&format!(
                    "msgid \"{escaped_english}\"\nmsgstr \"{escaped_translation}\"\n\n"
                ));
            }
        }

        let lc_messages_dir = output_dir.join(locale).join("LC_MESSAGES");
        std::fs::create_dir_all(&lc_messages_dir)?;
        write_if_changed(
            &lc_messages_dir.join("stremio-native.po"),
            po_content.as_bytes(),
        )?;
    }

    Ok(())
}

fn embed_windows_resources() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    println!("cargo:rerun-if-changed=assets/app.ico");
    let mut resources = winres::WindowsResource::new();
    resources.set_icon_with_id("assets/app.ico", "MAINICON");
    resources.set("FileDescription", "Stremio");
    resources.set("ProductName", "Stremio");
    resources.set("OriginalFilename", "stremio-native.exe");
    resources.compile()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=STREMIO_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-env-changed=CI_COMMIT_SHA");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rustc-env=STREMIO_BUILD_VERSION={}", build_version());
    embed_windows_resources()?;

    // 1. Generate Slint imports file for imported_fonts.slint
    let slint_imports =
        "// Generated automatically by build.rs. DO NOT EDIT.\nexport component Fonts {}\n";
    write_if_changed(
        std::path::Path::new("ui/imported_fonts.slint"),
        slint_imports.as_bytes(),
    )?;

    // 2. Generate translations PO files from vendor JSONs
    let translations_src_dir = std::path::Path::new("../vendor/stremio-translations");
    let out_dir = std::env::var("OUT_DIR")?;
    let translations_dest_dir = std::path::Path::new(&out_dir).join("translations");

    println!("cargo:rerun-if-changed=../vendor/stremio-translations");
    generate_po_files(translations_src_dir, &translations_dest_dir)?;

    // 3. Compile Slint UI with translations in a separate thread with a larger stack size
    // to prevent stack overflows on Windows (default 1MB stack size).
    let compile_thread = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024) // 16 MB
        .spawn(move || {
            let config = slint_build::CompilerConfiguration::new()
                .with_bundled_translations(translations_dest_dir)
                .with_default_translation_context(slint_build::DefaultTranslationContext::None);
            slint_build::compile_with_config("ui/app.slint", config)
                .map_err(|error| error.to_string())
        })?;
    compile_thread
        .join()
        .map_err(|_| std::io::Error::other("Slint compiler thread panicked"))?
        .map_err(std::io::Error::other)?;

    Ok(())
}
