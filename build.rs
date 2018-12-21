use shaderc::{Compiler, Error, ShaderKind};
use std::env;
use std::fs::{self, File};
use std::io::prelude::*;
use std::path::Path;
use walkdir::WalkDir;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let mut compiler = Compiler::new().unwrap();

    for entry in WalkDir::new("shaders").into_iter() {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            let path = entry.path();
            let kind = match path.extension().and_then(|ext| ext.to_str()) {
                Some("vert") => ShaderKind::Vertex,
                Some("frag") => ShaderKind::Fragment,
                _ => continue,
            };

            println!("cargo:rerun-if-changed={}", path.to_str().unwrap());
            let mut source = String::new();
            File::open(path).unwrap().read_to_string(&mut source).unwrap();
            let artifact = match compiler.compile_into_spirv(
                &source,
                kind,
                path.to_str().unwrap(),
                "main",
                None,
            ) {
                Ok(artifact) => artifact,
                Err(Error::CompilationError(_, err)) => {
                    panic!("Shader compilation failed:\n{}", err)
                },
                Err(err) => panic!("Shader compilation failed: {:?}", err),
            };
            if artifact.get_num_warnings() > 0 {
                println!("cargo:warning={}", artifact.get_warning_messages());
            }

            let mut ext = path.extension().unwrap().to_os_string();
            ext.push(".spirv");
            let dest_path = Path::new(&out_dir).join(path).with_extension(ext);
            fs::create_dir_all(dest_path.parent().unwrap()).unwrap();
            let mut file = File::create(&dest_path).unwrap();
            file.write_all(artifact.as_binary_u8()).unwrap();
        }
    }
}
