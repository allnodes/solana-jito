#[macro_export(local_inner_macros)]
macro_rules! __constants {
    ($(#[$a:meta])* ($($v:tt)*) const $n:ident : $t:ty = $e:expr; $($r:tt)*) => {
        $(#[$a])* $($v)* static $n: std::sync::LazyLock<$t> = std::sync::LazyLock::new(|| {
            $crate::CONSTANTS.read()
                .as_ref()
                .and_then(|c| c.get(std::stringify!($n)))
                .unwrap_or($e)
        });

        constants!($($r)*);
    };

    () => ()
}

#[macro_export(local_inner_macros)]
macro_rules! constants {
    ($(#[$a:meta])* const $n:ident : $t:ty = $e:expr; $($r:tt)*) => {
        __constants!($(#[$a])* () const $n : $t = $e; $($r)*);
    };

    ($(#[$a:meta])* pub const $n:ident : $t:ty = $e:expr; $($r:tt)*) => {
        __constants!($(#[$a])* (pub) const $n : $t = $e; $($r)*);
    };

    ($(#[$a:meta])* pub ($($v:tt)+) const $n:ident : $t:ty = $e:expr; $($r:tt)*) => {
        __constants!($(#[$a])* (pub ($($v)+)) const $n : $t = $e; $($r)*);
    };

    () => ()
}
