use std::{fmt::Display, path::Path};

pub fn is_existing_file<T>(value: T) -> Result<(), String>
where
    T: AsRef<str> + Display,
{
    let path = Path::new(value.as_ref());
    if !path.exists() {
        return Err(format!("File not found: {}", path.display()));
    }
    if !path.is_file() {
        return Err(format!("Must be a file: {}", path.display()));
    }
    Ok(())
}
