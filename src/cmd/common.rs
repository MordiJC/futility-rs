use camino::Utf8PathBuf;

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
