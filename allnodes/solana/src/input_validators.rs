use std::fmt::Display;

pub fn bool_validator<T>(value: T) -> Result<(), String>
where
    T: AsRef<str> + Display,
{
    match value.as_ref().trim() {
        "" | "true" | "false" => Ok(()),
        _ => Err(format!("Must be 'true' or 'false': {value}")),
    }
}
