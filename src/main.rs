extern crate chrono;
extern crate elf;
extern crate tar;
#[macro_use]
extern crate structopt;

use std::cmp;
use std::fmt::Write as fmtwrite;
use std::fs;
use std::io;
use std::io::{Seek, Write};
use std::mem;

#[macro_use]
mod util;
mod cmdline;
mod header;
use structopt::StructOpt;

fn main() {
    // 格式化命令
    let opt = cmdline::Opt::from_args();

    let package_name = opt
        .package_name
        .as_ref()
        .map_or("", |package_name| package_name.as_str());

    // Create the metadata.toml file needed for the TAB file.
    let mut metadata_toml = String::new();
    // 拼接一个这样的字符串："tab-version = 1\nname = \"blink\"\nonly-for-boards = \"\"\nbuild-date = 2020-05-13T14:55:10Z\n"
    writeln!(&mut metadata_toml, "tab-version = 1").unwrap();
    writeln!(&mut metadata_toml, "name = \"{}\"", package_name).unwrap();
    writeln!(&mut metadata_toml, "only-for-boards = \"\"").unwrap();
    if !opt.deterministic {
        let build_date = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        writeln!(&mut metadata_toml, "build-date = {}", build_date).unwrap();
    }

    // Start creating a tar archive which will be the .tab file.
    // 根据文件名创建一个文件
    let tab_name = fs::File::create(&opt.output).expect("Could not create the output file.");
    let mut tab = tar::Builder::new(tab_name);
    tab.mode(tar::HeaderMode::Deterministic);

    // Add the metadata file without creating a real file on the filesystem.
    // 不在真正的文件系统里面添加metadata，主要用于存放构建信息包括：tab版本/包名/适合的主板/构建日期
    // metadata的说明：https://github.com/tock/tock/blob/74b8693a903aa187f7832ea6bda85265690ecd76/doc/Compilation.md#metadata
    let mut header = tar::Header::new_gnu();
    header.set_size(metadata_toml.as_bytes().len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tab.append_data(&mut header, "metadata.toml", metadata_toml.as_bytes())
        .unwrap();

    // Iterate all input elfs. Convert them to Tock friendly binaries and then
    // add them to the TAB file.
    // 遍历所有的elf文件，然后将他们转化为tfb文件，在将他们统一存放到tab文件里面
    for elf_path in opt.input {
        // 改成tbf后缀
        let tbf_path = elf_path.with_extension("tbf");
        // 使用elf包读取elf文件
        let elffile = elf::File::open_path(&elf_path).expect("Could not open the .elf file.");

        if opt.output.clone() == tbf_path.clone() {
            panic!(
                "tab file {} and output file {} cannot be the same file",
                opt.output.clone().to_str().unwrap(),
                tbf_path.to_str().unwrap()
            );
        }

        // Get output file as both read/write for creating the binary and
        // adding it to the TAB tar file.
        // 复制并创建出可读可写的tar文件
        let mut outfile: fs::File = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(tbf_path.clone())
            .unwrap();

        // Do the conversion to a tock binary.
        // 将elf文件转化为tab文件
        elf_to_tbf(
            &elffile,
            &mut outfile,
            opt.package_name.clone(),
            opt.verbose,
            opt.stack_size,
            opt.app_heap_size,
            opt.kernel_heap_size,
            opt.protected_region_size,
        )
        .unwrap();

        // Add the file to the TAB tar file.
        outfile.seek(io::SeekFrom::Start(0)).unwrap();
        tab.append_file(tbf_path.file_name().unwrap(), &mut outfile)
            .unwrap();
        outfile.seek(io::SeekFrom::Start(0)).unwrap();
        tab.append_file(
            tbf_path.with_extension("bin").file_name().unwrap(),
            &mut outfile,
        )
        .unwrap();
    }
}

/// Convert an ELF file to a TBF (Tock Binary Format) binary file.
///
/// This will place all writeable and executable sections from the ELF file
/// into a binary and prepend a TBF header to it. For all writeable sections,
/// if there is a .rel.X section it will be included at the end with a 32 bit
/// length parameter first.
/// 这会将ELF文件中的所有可写和可执行部分放入二进制文件中，并在其前面添加一个TBF标头。
///  对于所有可写节，如果有一个.rel.X节，它将在末尾包含32位长度的参数。
///
/// Assumptions:
/// - Sections in a segment that is RW and set to be loaded will be in RAM and
///   should count towards minimum required RAM.
/// - Sections that are writeable flash regions include .wfr in their name.
// 假设：
// -RW段中设置为要加载的段将位于RAM中，并应计入所需的最小RAM中。
// -可写闪存区域的部分名称中包括.wfr。
//

// input就是elf的File格式文件
fn elf_to_tbf<W: Write>(
    input: &elf::File,
    output: &mut W,
    package_name: Option<String>,
    verbose: bool,
    stack_len: u32,
    app_heap_len: u32,
    kernel_heap_len: u32,
    protected_region_size_arg: Option<u32>,
) -> io::Result<()> {
    let package_name = package_name.unwrap_or_default();

    // Get an array of the sections sorted so we place them in the proper order
    // in the binary.
    // 遍历elf的section并且排序
    let mut sections_sort: Vec<(usize, usize)> = Vec::new();
    for (i, section) in input.sections.iter().enumerate() {
        sections_sort.push((i, section.shdr.offset as usize));
    }
    sections_sort.sort_by_key(|s| s.1);

    // Keep track of how much RAM this app will need.
    // 追踪这个app需要的最小的RAM
    let mut minimum_ram_size: u32 = 0;

    // Find the ELF segment for the RAM segment. That will tell us how much
    // RAM we need to reserve for when those are copied into memory.
    // 为了知道需要多少RAM，复制的时候找出elf文件的elf segment, 用elf segment去设置RAM segment。
    for segment in &input.phdrs {
        if segment.progtype == elf::types::PT_LOAD
            && segment.flags.0 == elf::types::PF_W.0 + elf::types::PF_R.0
        {
            minimum_ram_size = segment.memsz as u32;
            break;
        }
    }
    if verbose {
        println!(
            "Min RAM size from sections in ELF: {} bytes",
            minimum_ram_size
        );
    }

    // Add in room the app is asking us to reserve for the stack and heaps to
    // the minimum required RAM size.
    // 除了segment区，还要添加栈/堆/和内核保留空间，并最终计算得到最小内存
    minimum_ram_size += align8!(stack_len) + align4!(app_heap_len) + align4!(kernel_heap_len);

    // Need an array of sections to look for relocation data to include.
    // 需要一个数组来查找要包括的重定位数据。
    let mut rel_sections: Vec<String> = Vec::new();

    // Iterate the sections in the ELF file to find properties of the app that
    // are required to go in the TBF header.
    // 遍历ELF文件中的部分，以查找进入TBF标头所需的应用程序属性。
    let mut writeable_flash_regions_count = 0;

    for s in &sections_sort {
        let section = &input.sections[s.0];

        // Count write only sections as writeable flash regions.
        // 计算只能写的sections作为可写的flash寄存器
        if section.shdr.name.contains(".wfr") && section.shdr.size > 0 {
            writeable_flash_regions_count += 1;
        }

        // Check write+alloc sections for possible .rel.X sections.
        // 检查可写可分配的section给.rel.x
        if section.shdr.flags.0 == elf::types::SHF_WRITE.0 + elf::types::SHF_ALLOC.0 {
            // This section is also one we might need to include relocation
            // data for.
            rel_sections.push(section.shdr.name.clone());
        }
    }
    if verbose {
        println!(
            "Number of writeable flash regions: {}",
            writeable_flash_regions_count
        );
    }

    // Keep track of an index of where we are in creating the app binary.
    let mut binary_index = 0;

    // Now we can create the first pass TBF header. This is mostly to get the
    // size of the header since we have to fill in some of the offsets later.
    let mut tbfheader = header::TbfHeader::new();
    let header_length = tbfheader.create(
        minimum_ram_size,
        writeable_flash_regions_count,
        package_name,
    );
    // If a protected region size was passed, confirm the header will fit.
    // Otherwise, use the header size as the protected region size.
    // 如果通过了受保护的区域大小，请确认标题适合。 否则，将标头大小用作保护区域大小。
    let protected_region_size =
        if let Some(fixed_protected_region_size) = protected_region_size_arg {
            if fixed_protected_region_size < header_length as u32 {
                // The header doesn't fit in the provided protected region size;
                // throw an error.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                    "protected_region_size = {} is too small for the TBF headers. Header size: {}",
                    fixed_protected_region_size, header_length),
                ));
            }
            // Update the header's protected size, as the protected region may
            // be larger than the header size.
            tbfheader.set_protected_size(fixed_protected_region_size - header_length as u32);

            fixed_protected_region_size
        } else {
            header_length as u32
        };
    binary_index += protected_region_size as usize;

    // The init function is where the app will start executing, defined as an
    // offset from the end of protected region at the beginning of the app in
    // flash. Typically the protected region only includes the TBF header. To
    // calculate the offset we need to find which section includes the entry
    // function and then determine its offset relative to the end of the
    // protected region.
    let mut init_fn_offset: u32 = 0;

    // Need a place to put the app sections before we know the true TBF header.
    let mut binary: Vec<u8> = vec![0; protected_region_size as usize - header_length];

    let mut entry_point_found = false;

    // Iterate the sections in the ELF file and add them to the binary as needed
    for s in &sections_sort {
        let section = &input.sections[s.0];

        // Determine if this is the section where the entry point is in. If it
        // is, then we need to calculate the correct init_fn_offset.
        if input.ehdr.entry >= section.shdr.addr
            && input.ehdr.entry < (section.shdr.addr + section.shdr.size)
            && (section.shdr.name.find("debug")).is_none()
        {
            // panic in case we detect entry point in multiple sections.
            if entry_point_found {
                panic!("Duplicate entry point in {} section", section.shdr.name);
            }
            entry_point_found = true;

            if verbose {
                println!("Entry point is in {} section", section.shdr.name);
            }
            // init_fn_offset is specified relative to the end of the TBF
            // header.
            init_fn_offset = (input.ehdr.entry - section.shdr.addr) as u32
                + (binary_index - header_length) as u32
        }

        // If this is writeable, executable, or allocated, is nonzero length,
        // and is type `PROGBITS` we want to add it to the binary.
        if (section.shdr.flags.0
            & (elf::types::SHF_WRITE.0 + elf::types::SHF_EXECINSTR.0 + elf::types::SHF_ALLOC.0)
            != 0)
            && section.shdr.shtype == elf::types::SHT_PROGBITS
            && section.shdr.size > 0
        {
            if verbose {
                println!(
                    "  Adding {0} section. Offset: {1} ({1:#x}). Length: {2} ({2:#x}) bytes.",
                    section.shdr.name,
                    binary_index,
                    section.data.len(),
                );
            }
            if align4needed!(binary_index) != 0 {
                println!(
                    "Warning! Placing section {} at {:#x}, which is not 4-byte aligned.",
                    section.shdr.name, binary_index
                );
            }
            binary.extend(&section.data);

            // Check if this is a writeable flash region. If so, we need to
            // set the offset and size in the header.
            if section.shdr.name.contains(".wfr") && section.shdr.size > 0 {
                tbfheader.set_writeable_flash_region_values(
                    binary_index as u32,
                    section.shdr.size as u32,
                );
            }

            // Now increment where we are in the binary.
            binary_index += section.shdr.size as usize;
        }
    }

    // Now that we have checked all of the sections, we can set the
    // init_fn_offset.
    tbfheader.set_init_fn_offset(init_fn_offset);

    // Next we have to add in any relocation data.
    let mut relocation_binary: Vec<u8> = Vec::new();

    // For each section that might have relocation data, check if a .rel.X
    // section exists and if so include it.
    if verbose {
        println!("Searching for .rel.X sections to add.");
    }
    for relocation_section_name in &rel_sections {
        let mut name: String = ".rel".to_owned();
        name.push_str(relocation_section_name);

        let rel_data = input
            .sections
            .iter()
            .find(|section| section.shdr.name == name)
            .map_or(&[] as &[u8], |section| section.data.as_ref());

        relocation_binary.extend(rel_data);

        if verbose && !rel_data.is_empty() {
            println!(
                "  Adding {0} section. Offset: {1} ({1:#x}). Length: {2} ({2:#x}) bytes.",
                name,
                binary_index + mem::size_of::<u32>() + rel_data.len(),
                rel_data.len(),
            );
        }
        if !rel_data.is_empty() && align4needed!(binary_index) != 0 {
            println!(
                "Warning! Placing section {} at {:#x}, which is not 4-byte aligned.",
                name, binary_index
            );
        }
    }

    // Add the relocation data to our total length. Also include the 4 bytes for
    // the relocation data length.
    binary_index += relocation_binary.len() + mem::size_of::<u32>();

    // That is everything that we are going to include in our app binary. Now
    // we need to pad the binary to a power of 2 in size, and make sure it is
    // at least 512 bytes in size.
    let post_content_pad = if binary_index.count_ones() > 1 {
        let power2len = cmp::max(1 << (32 - (binary_index as u32).leading_zeros()), 512);
        power2len - binary_index
    } else {
        0
    };
    binary_index += post_content_pad;
    let total_size = binary_index;

    // Now set the total size of the app in the header.
    tbfheader.set_total_size(total_size as u32);

    if verbose {
        print!("{}", tbfheader);
    }

    // Write the header and actual app to a binary file.
    output.write_all(tbfheader.generate().unwrap().get_ref())?;
    output.write_all(binary.as_ref())?;

    let rel_data_len: [u8; 4] = (relocation_binary.len() as u32).to_le_bytes();
    output.write_all(&rel_data_len)?;
    output.write_all(relocation_binary.as_ref())?;

    // Pad to get a power of 2 sized flash app.
    util::do_pad(output, post_content_pad as usize)?;

    Ok(())
}
