use camino::Utf8PathBuf;
use std::str::FromStr;

pub fn file_exists_validator(s: &str) -> Result<Utf8PathBuf, String> {
    let p = Utf8PathBuf::from(s);
    if p.exists() {
        Ok(p)
    } else {
        Err(format!("File '{}' does not exist", s).to_string())
    }
}

pub fn area_to_file_mapping_param_valid(s: &str) -> Result<(String, Utf8PathBuf), String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return Err(String::from(
            "The argument should be in the format 'SECTION:PATH'",
        ));
    }
    Ok((String::from(parts[0]), Utf8PathBuf::from(parts[1])))
}

pub fn decimal_or_hex_validator_u8(s: &str) -> Result<u8, String> {
    if let Ok(decimal) = u8::from_str(s) {
        return Ok(decimal);
    }
    let s1 = if s.starts_with("0x") {
        s.strip_prefix("0x").unwrap()
    } else if s.starts_with("0X") {
        s.strip_prefix("0X").unwrap()
    } else {
        s
    };
    if let Ok(hex) = u8::from_str_radix(s1, 16) {
        return Ok(hex);
    }
    Err(format!(
        "Value '{s}' is not a correctr integer nor hex value matching the argument type"
    ))
}
