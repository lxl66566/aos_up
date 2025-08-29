use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use palc::{Parser, Subcommand};

// C++ 代码中的 #pragma pack(1) 在 Rust 中用 #[repr(C, packed)] 实现
// 我们需要确保内存布局与 C++ 版本完全一致

const ARCHIVE_NAME_SIZE: usize = 261;
const FILENAME_SIZE: usize = 32;

#[repr(C, packed)]
#[derive(Debug)]
struct AosV2Hdr {
    unknown1: u32,
    data_offset: u32, // 在原始代码中未使用，但封包时可以填充为0
    toc_length: u32,
    archive_name: [u8; ARCHIVE_NAME_SIZE],
}

impl AosV2Hdr {
    fn from_reader<R: Read>(reader: &mut R) -> io::Result<Self> {
        let mut buffer = [0u8; std::mem::size_of::<Self>()];
        reader.read_exact(&mut buffer)?;
        // 使用 unsafe 是因为我们正在从原始字节转换，必须确保类型布局正确
        Ok(unsafe { std::ptr::read(buffer.as_ptr() as *const _) })
    }

    fn to_bytes(&self) -> Vec<u8> {
        let size = std::mem::size_of::<Self>();
        let mut bytes = Vec::with_capacity(size);
        // 使用 unsafe 将结构体转换为字节切片
        unsafe {
            let ptr = self as *const Self as *const u8;
            bytes.extend_from_slice(std::slice::from_raw_parts(ptr, size));
        }
        bytes
    }
}

#[repr(C, packed)]
#[derive(Debug)]
struct AosV2Entry {
    filename: [u8; FILENAME_SIZE],
    offset: u32,
    length: u32,
}

impl AosV2Entry {
    fn from_reader<R: Read>(reader: &mut R) -> io::Result<Self> {
        let mut buffer = [0u8; std::mem::size_of::<Self>()];
        reader.read_exact(&mut buffer)?;
        Ok(unsafe { std::ptr::read(buffer.as_ptr() as *const _) })
    }

    fn to_bytes(&self) -> Vec<u8> {
        let size = std::mem::size_of::<Self>();
        let mut bytes = Vec::with_capacity(size);
        unsafe {
            let ptr = self as *const Self as *const u8;
            bytes.extend_from_slice(std::slice::from_raw_parts(ptr, size));
        }
        bytes
    }

    // 辅助函数，用于从字节数组中获取文件名字符串
    fn get_filename_str(&self) -> Result<String> {
        let null_pos = self
            .filename
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(FILENAME_SIZE);
        String::from_utf8(self.filename[..null_pos].to_vec()).context("文件名包含无效的 UTF-8 字符")
    }
}

/// 解包 .aos 文件
fn unpack_archive(archive_path: &Path) -> Result<()> {
    println!("正在解包: {}", archive_path.display());

    let mut file = File::open(archive_path)
        .with_context(|| format!("无法打开文件: {}", archive_path.display()))?;

    // 1. 读取文件头
    let header = AosV2Hdr::from_reader(&mut file)?;

    // 2. 读取目录表 (TOC)
    let entry_count = header.toc_length as usize / std::mem::size_of::<AosV2Entry>();
    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        entries.push(AosV2Entry::from_reader(&mut file)?);
    }

    // 3. 创建输出目录
    let output_dir_name = archive_path.file_stem().unwrap_or_default();
    let output_dir = archive_path.with_file_name(output_dir_name);
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("无法创建目录: {}", output_dir.display()))?;

    println!("解包到目录: {}", output_dir.display());

    // 4. 计算数据区基地址并提取文件
    let base_offset = (std::mem::size_of::<AosV2Hdr>() + header.toc_length as usize) as u64;

    for entry in &entries {
        let filename_str = entry.get_filename_str()?;
        let output_path = output_dir.join(&filename_str);

        println!("  -> 提取: {filename_str}");

        let mut buffer = vec![0u8; entry.length as usize];
        file.seek(SeekFrom::Start(base_offset + entry.offset as u64))?;
        file.read_exact(&mut buffer)?;

        fs::write(&output_path, &buffer)
            .with_context(|| format!("无法写入文件: {}", output_path.display()))?;
    }

    println!("解包完成。");
    Ok(())
}

/// 封包一个目录
fn pack_directory(dir_path: &Path) -> Result<()> {
    println!("正在封包目录: {}", dir_path.display());

    let files_to_pack: Vec<PathBuf> = fs::read_dir(dir_path)
        .with_context(|| format!("无法读取目录: {}", dir_path.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();

    if files_to_pack.is_empty() {
        bail!("目录为空，没有可封包的文件。");
    }

    // 1. 构建目录表 (TOC) 和计算数据区
    let mut entries = Vec::new();
    let mut data_blob = Vec::new();
    let mut current_offset = 0u32;

    for file_path in &files_to_pack {
        let filename = file_path
            .file_name()
            .and_then(|s| s.to_str())
            .context("文件名无效")?;

        if filename.len() >= FILENAME_SIZE {
            bail!(
                "文件名 '{}' 过长 (最大 {} 字节)",
                filename,
                FILENAME_SIZE - 1
            );
        }

        let file_data = fs::read(file_path)?;
        let file_length = file_data.len() as u32;

        let mut filename_bytes = [0u8; FILENAME_SIZE];
        filename_bytes[..filename.len()].copy_from_slice(filename.as_bytes());

        let entry = AosV2Entry {
            filename: filename_bytes,
            offset: current_offset,
            length: file_length,
        };
        entries.push(entry);

        data_blob.extend_from_slice(&file_data);
        current_offset += file_length;
    }

    // 2. 构建文件头
    let dir_name = dir_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("archive");
    let archive_name_str = format!("{dir_name}.aos");
    let mut archive_name_bytes = [0u8; ARCHIVE_NAME_SIZE];
    // 确保不会因为文件名过长而 panic
    let name_len = std::cmp::min(archive_name_str.len(), ARCHIVE_NAME_SIZE - 1);
    archive_name_bytes[..name_len].copy_from_slice(&archive_name_str.as_bytes()[..name_len]);

    let toc_length = (entries.len() * std::mem::size_of::<AosV2Entry>()) as u32;
    let header_size = std::mem::size_of::<AosV2Hdr>() as u32;

    let header = AosV2Hdr {
        unknown1: 0,
        data_offset: header_size + toc_length,
        toc_length,
        archive_name: archive_name_bytes,
    };

    // 3. 写入到 .aos 文件
    let output_filename = dir_path.with_extension("aos");
    let mut output_file = File::create(&output_filename)
        .with_context(|| format!("无法创建输出文件: {}", output_filename.display()))?;

    // 写入文件头
    output_file.write_all(&header.to_bytes())?;

    // 写入目录表
    for entry in &entries {
        output_file.write_all(&entry.to_bytes())?;
    }

    // 写入文件数据
    output_file.write_all(&data_blob)?;

    println!("封包完成，输出文件: {}", output_filename.display());
    Ok(())
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 解包 .aos 文件
    Unpack {
        /// 要解包的 .aos 文件路径
        #[arg(value_name = "FILE")]
        archive_path: PathBuf,
    },
    /// 封包一个目录
    Pack {
        /// 要封包的目录路径
        #[arg(value_name = "DIRECTORY")]
        dir_path: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Unpack { archive_path } => {
            if !archive_path.exists() || !archive_path.is_file() {
                bail!(
                    "错误: 文件 '{}' 不存在或不是一个有效的文件。",
                    archive_path.display()
                );
            }
            unpack_archive(archive_path)?;
        }
        Commands::Pack { dir_path } => {
            if !dir_path.exists() || !dir_path.is_dir() {
                bail!(
                    "错误: 目录 '{}' 不存在或不是一个有效的目录。",
                    dir_path.display()
                );
            }
            pack_directory(dir_path)?;
        }
    }

    Ok(())
}
