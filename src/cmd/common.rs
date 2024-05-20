use camino::Utf8PathBuf;

pub fn file_exists_validator(s: &str) -> Result<Utf8PathBuf, String> {
    let p = Utf8PathBuf::from(s);
    if p.exists() {
        Ok(p)
    } else {
        Err(format!("File '{}' does not exist", s).to_string())
    }
}
