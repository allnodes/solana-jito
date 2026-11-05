#[macro_export(local_inner_macros)]
macro_rules! __constants {
    ($(#[$a:meta])* ($($v:tt)*) const $n:ident : $t:ty = $e:expr; $($r:tt)*) => {
        $(#[$a])* $($v)* static $n: $crate::WrappedConst<$t> = $crate::WrappedConst::new(|| {
            $crate::CONSTANTS.register(std::stringify!($n), $e)
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
