fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon.png");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let icon = output.join("diri.ico");
    image::open("../../assets/icon.png")
        .expect("application icon")
        .resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
        .save_with_format(&icon, image::ImageFormat::Ico)
        .expect("Windows icon");
    let version = std::env::var("CARGO_PKG_VERSION").expect("version");
    let components = version
        .split('.')
        .map(|s| s.parse::<u16>().expect("numeric release version"))
        .collect::<Vec<_>>();
    assert_eq!(components.len(), 3);
    let numeric = format!("{},{},{},0", components[0], components[1], components[2]);
    let icon = icon.to_string_lossy().replace('\\', "\\\\");
    let resource = output.join("diri.rc");
    std::fs::write(
        &resource,
        format!(
            r#"
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004
FILETYPE 1
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904B0"
  BEGIN
   VALUE "FileDescription", "Diri\0"
   VALUE "FileVersion", "{version}\0"
   VALUE "ProductName", "Diri\0"
   VALUE "ProductVersion", "{version}\0"
   VALUE "OriginalFilename", "diri.exe\0"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x409, 1200
 END
END
"#
        ),
    )
    .expect("Windows version resource");
    embed_resource::compile(&resource, embed_resource::NONE)
        .manifest_required()
        .expect("Windows resource compiler");
}
