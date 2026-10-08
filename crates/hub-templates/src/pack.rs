//! 把渲染后的模板打成 zip。
//!
//! 用 `zip` crate 而不是手写：CRC32、中央目录偏移、UTF-8 文件名标志位这些边界
//! 不值得自己维护，而它们出错时表现为「某些解压工具打不开」，很难查。
//! 关闭了默认 feature，只做 store（不压缩）——模板都是几十 KB 的文本，
//! 压缩率不值得为它引入 deflate 及其依赖。

use std::io::{Cursor, Write};

use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::Language;
use crate::render::{RenderError, Values, render};

/// 打包失败。
#[derive(Debug)]
pub enum PackError {
    /// 某个模板渲染失败。带上文件名——同一个占位符可能出现在多个文件里。
    Render { file: String, source: RenderError },

    /// zip 写入失败（正常不会发生，写的是内存）。
    Zip(zip::result::ZipError),

    /// 写内存失败（理论上不会发生，但不写 unwrap）。
    Io(std::io::Error),
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Render { file, source } => write!(f, "渲染 {file} 失败：{source}"),
            Self::Zip(e) => write!(f, "打包失败：{e}"),
            Self::Io(e) => write!(f, "打包失败：{e}"),
        }
    }
}

impl std::error::Error for PackError {}

impl From<zip::result::ZipError> for PackError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e)
    }
}

impl From<std::io::Error> for PackError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// 下载包的文件名。
pub fn download_filename(lang: &Language, plugin_name: &str) -> String {
    format!("plugin-hub-{}-plugin-{plugin_name}.zip", lang.id)
}

/// 渲染并打包一门语言的插件工程。
///
/// 包内顶层是一层以插件名命名的目录：解压时不会把文件散到当前目录里，
/// 这是 `zip` 分发的惯例，也是开发者解压后立刻能 `cd` 进去的前提。
pub fn build_zip(lang: &Language, values: &Values) -> Result<Vec<u8>, PackError> {
    // `@@sdk_path@@` 由这里按语言注入，而不是让调用方传：包内 SDK 的目录名是
    // 语言的属性（`sdk`），调用方没有理由知道它，也没有理由能把它传错。
    let mut values = values.clone();
    if !lang.sdk_dir.is_empty() {
        values.sdk_path = format!("./{}", lang.sdk_dir);
    }
    let values = &values;

    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    // store 模式：模板是纯文本且很小，压缩收益抵不上引入 deflate 的代价
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    for (output, raw) in lang.files {
        let raw = std::str::from_utf8(raw).map_err(|e| {
            PackError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("模板 {output} 不是 UTF-8：{e}"),
            ))
        })?;

        let rendered = render(raw, values).map_err(|source| PackError::Render {
            file: (*output).to_string(),
            source,
        })?;

        writer.start_file(format!("{}/{output}", values.name), options)?;
        writer.write_all(rendered.as_bytes())?;
    }

    // 随包下发的 SDK 源码：**原样写入，不渲染**。SDK 不是模板——它里面没有占位符，
    // 而且让一份源码走模板渲染只会在它哪天含了 `@@` 时莫名其妙地报错。
    if !lang.sdk_dir.is_empty() {
        for (path, bytes) in lang.sdk_files {
            writer.start_file(format!("{}/{}/{path}", values.name, lang.sdk_dir), options)?;
            writer.write_all(bytes)?;
        }
    }

    Ok(writer.finish()?.into_inner())
}

/// 打包后会是多少字节。控制台的卡片上要显示包大小。
///
/// 直接打一次再量，而不是把各文件长度加起来：后者算的是**内容**大小，
/// 而下载到的是 zip 大小，两者差着每个条目的头部与中央目录——显示一个
/// 与实际下载量对不上的数字，比不显示更糟。
pub fn archive_size(lang: &Language, values: &Values) -> Result<usize, PackError> {
    Ok(build_zip(lang, values)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languages;

    fn values() -> Values {
        Values {
            name: "order-reader".into(),
            package: "example.com/order-reader".into(),
            sdk_module: "example.com/sdk".into(),
            sdk_path: "./sdk".into(),
            hub_addr: "http://127.0.0.1:8093".into(),
            onboarding: "（共通层正文）".into(),
        }
    }

    fn go() -> &'static Language {
        languages()
            .iter()
            .copied()
            .find(|l| l.id == "go")
            .expect("go 模板必须存在")
    }

    #[test]
    fn 打出来的包能被解压且文件齐全() {
        let bytes = build_zip(go(), &values()).unwrap();

        // 用 zip 自己的读取端回读：这验的是「产出的是一份合法 zip」，
        // 而不是「我们写进去的字节数对得上」。解压不了的话，下载页就是个死链。
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("产出的必须是合法 zip");

        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        for (output, _) in go().files {
            let want = format!("order-reader/{output}");
            assert!(names.contains(&want), "包里缺少 {want}；实际有 {names:?}");
        }
        assert_eq!(
            names.len(),
            go().files.len() + go().sdk_files.len(),
            "包里的条目数应是「模板文件 + 随包 SDK 源码」"
        );
    }

    #[test]
    fn 随包sdk在包内且排除了那两样() {
        let bytes = build_zip(go(), &values()).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        // 生成的工程要能编，SDK 的入口文件必须在
        for must in [
            "order-reader/sdk/go.mod",
            "order-reader/sdk/hubkit/run.go",
            "order-reader/sdk/proto/hubv1/plugin.pb.go",
        ] {
            assert!(names.contains(&must.to_string()), "包里缺少 {must}");
        }

        // vendor/：15MB，且它只对 SDK 自己有意义——新工程是一个新 module，
        // 用别人的 vendor 会被 Go 判为 inconsistent vendoring 直接拒绝。
        assert!(
            !names.iter().any(|n| n.contains("/sdk/vendor/")),
            "包内 SDK 不该带 vendor/"
        );

        // cmd/hub-plugin：脚手架自己，带 //go:embed all:templates，而 templates
        // 不随源码发出——带进去开发者拿到手**编译直接失败**。
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with("order-reader/sdk/cmd/hub-plugin/")),
            "包内 SDK 不该带 cmd/hub-plugin/——它会因缺 templates 而编译失败"
        );

        // 反过来，hubprobe 要在：开发者靠它跑 L2
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("order-reader/sdk/cmd/hubprobe/")),
            "包内 SDK 应带 cmd/hubprobe/——它是开发者跑 L2 要用的工具"
        );
    }

    #[test]
    fn 模板文件渲染干净而_sdk_原样照抄() {
        let bytes = build_zip(go(), &values()).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();

        let sdk_prefix = format!("order-reader/{}/", go().sdk_dir);

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            let name = entry.name().to_string();
            let mut text = String::new();
            std::io::Read::read_to_string(&mut entry, &mut text).unwrap();

            // SDK 是原样照抄的源码，不该拿模板的规则去要求它——
            // 它里面出现 `@@` 也好、出现插件名也罢，都正常。
            if name.starts_with(&sdk_prefix) {
                continue;
            }

            assert!(
                !text.contains(crate::render::PLACEHOLDER),
                "{name} 里残留了未渲染的占位符"
            );
            assert!(
                !text.contains("order-reader/order-reader"),
                "{name} 里的路径被重复拼接了"
            );
        }
    }

    #[test]
    fn 包内顶层是一层以插件名命名的目录() {
        let bytes = build_zip(go(), &values()).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();

        for i in 0..archive.len() {
            let name = archive.by_index(i).unwrap().name().to_string();
            assert!(
                name.starts_with("order-reader/"),
                "{name} 不在 order-reader/ 目录下——解压时会把文件散到当前目录"
            );
        }
    }

    #[test]
    fn 渲染失败时报出是哪个文件() {
        // 造一份含未知占位符的模板，确认错误信息点了文件名——
        // 同一个占位符可能出现在多个文件里，不指名道姓就得逐个去找
        let bad = Language {
            id: "test",
            display_name: "Test",
            description: "",
            package_hint: "",
            files: &[("a.txt", b"ok"), ("b.txt", b"@@nope@@")],
            source_dir: "",
            sdk_dir: "",
            sdk_files: &[],
            sdk_module: "example.com/sdk",
        };
        let err = build_zip(&bad, &values()).unwrap_err();
        match err {
            PackError::Render { file, source } => {
                assert_eq!(file, "b.txt");
                assert!(source.to_string().contains("nope"));
            }
            other => panic!("应是渲染失败，实际 {other:?}"),
        }
    }

    #[test]
    fn 下载文件名带语言与插件名() {
        assert_eq!(
            download_filename(go(), "order-reader"),
            "plugin-hub-go-plugin-order-reader.zip"
        );
    }

    #[test]
    fn 包大小与包本身一致() {
        let bytes = build_zip(go(), &values()).unwrap();
        assert_eq!(archive_size(go(), &values()).unwrap(), bytes.len());
    }
}
