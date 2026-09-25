use std::env;
use std::path::Path;
use std::process::ExitCode;

use autodirector_fix::autodirector_signature::analyze_autodirector_view_override;

const DOS_SIGNATURE: u16 = 0x5a4d;
const PE_SIGNATURE: u32 = 0x0000_4550;
const DOS_LFANEW_OFFSET: usize = 0x3c;
const COFF_TIMESTAMP_OFFSET: usize = 0x08;
const COFF_NUMBER_OF_SECTIONS_OFFSET: usize = 0x06;
const COFF_SIZE_OF_OPTIONAL_HEADER_OFFSET: usize = 0x14;
const PE_HEADERS_SIZE: usize = 0x18;
const SECTION_HEADER_SIZE: usize = 0x28;
const TEXT_SECTION_NAME: &[u8; 8] = b".text\0\0\0";

struct TextSection<'a> {
    rva: usize,
    data: &'a [u8],
}

fn main() -> ExitCode {
    let Some(path) = env::args_os().nth(1) else {
        eprintln!("usage: signature-report <path-to-client.dll> [build-id]");
        return ExitCode::from(64);
    };
    let build_id = env::args().nth(2);
    let path = Path::new(&path);

    let image = match std::fs::read(path) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("failed to read {}: {error}", path.display());
            return ExitCode::from(66);
        }
    };
    let text = match text_section_from_file(&image) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("failed to parse {}: {error:?}", path.display());
            return ExitCode::from(65);
        }
    };
    let report = analyze_autodirector_view_override(text.data);
    let timestamp = coff_timestamp(&image);

    println!(
        "{{\n  \"input\": \"{}\",\n  \"build_id\": {},\n  \"file_size\": {},\n  \"coff_timestamp\": {},\n  \"text_rva\": \"0x{:x}\",\n  \"text_size\": {},\n  \"autodirector_view_override\": {{\n    \"status\": \"{}\",\n    \"matches\": {},\n    \"patch_rva\": {},\n    \"view_setup_move\": {},\n    \"get_observer_state_rva\": {},\n    \"normal_setup_jump_target_rva\": {}\n  }}\n}}",
        json_escape(&path.display().to_string()),
        json_optional_string(build_id.as_deref()),
        image.len(),
        json_optional_u32(timestamp),
        text.rva,
        text.data.len(),
        report.status.as_str(),
        report.matches,
        json_optional_rva(report.patch_offset.map(|offset| text.rva + offset)),
        json_optional_move(report.view_setup_argument_move),
        json_optional_rva(
            report
                .get_observer_state_offset
                .map(|offset| text.rva + offset)
        ),
        json_optional_rva(
            report
                .original_jump_target_offset
                .map(|offset| text.rva + offset)
        ),
    );

    if report.is_compatible() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn json_optional_string(value: Option<&str>) -> String {
    value.map_or_else(
        || "null".to_string(),
        |value| format!("\"{}\"", json_escape(value)),
    )
}

fn json_optional_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "null".to_string(), |value| value.to_string())
}

fn json_optional_rva(value: Option<usize>) -> String {
    value.map_or_else(|| "null".to_string(), |value| format!("\"0x{value:x}\""))
}

fn json_optional_move(value: Option<[u8; 3]>) -> String {
    value.map_or_else(
        || "null".to_string(),
        |[a, b, c]| format!("\"{a:02x} {b:02x} {c:02x}\""),
    )
}

fn coff_timestamp(image: &[u8]) -> Option<u32> {
    let pe_header = pe_header_offset(image)?;
    read_u32(image, pe_header + COFF_TIMESTAMP_OFFSET)
}

fn text_section_from_file(image: &[u8]) -> Result<TextSection<'_>, &'static str> {
    let pe_header = pe_header_offset(image).ok_or("missing PE header")?;
    let section_count = read_u16(image, pe_header + COFF_NUMBER_OF_SECTIONS_OFFSET)
        .ok_or("missing section count")? as usize;
    let optional_header_size = read_u16(image, pe_header + COFF_SIZE_OF_OPTIONAL_HEADER_OFFSET)
        .ok_or("missing optional header size")? as usize;
    let section_table = pe_header
        .checked_add(PE_HEADERS_SIZE)
        .and_then(|offset| offset.checked_add(optional_header_size))
        .ok_or("section table overflow")?;

    for index in 0..section_count {
        let header = section_table
            .checked_add(index * SECTION_HEADER_SIZE)
            .ok_or("section header overflow")?;
        if image.get(header..header + 8) != Some(TEXT_SECTION_NAME) {
            continue;
        }
        let virtual_size = read_u32(image, header + 0x08).ok_or("missing virtual size")? as usize;
        let rva = read_u32(image, header + 0x0c).ok_or("missing section RVA")? as usize;
        let raw_size = read_u32(image, header + 0x10).ok_or("missing raw size")? as usize;
        let raw_offset = read_u32(image, header + 0x14).ok_or("missing raw offset")? as usize;
        let size = virtual_size.min(raw_size);
        let data = image
            .get(
                raw_offset
                    ..raw_offset
                        .checked_add(size)
                        .ok_or("section size overflow")?,
            )
            .ok_or(".text is outside the file")?;
        return Ok(TextSection { rva, data });
    }
    Err(".text section is missing")
}

fn pe_header_offset(image: &[u8]) -> Option<usize> {
    (read_u16(image, 0)? == DOS_SIGNATURE).then_some(())?;
    let pe_header = read_u32(image, DOS_LFANEW_OFFSET)? as usize;
    (read_u32(image, pe_header)? == PE_SIGNATURE).then_some(pe_header)
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
