//! Declaration of a fieldless enum together with its closed `ALL` inventory.

/// Declares a fieldless enum and its `ALL` inventory from one variant list.
///
/// The variant declaration and its membership in `ALL` both come from the same list, so a new variant reaches the
/// inventory by construction and the two cannot drift.
macro_rules! closed_inventory {
    (
        $(#[$enum_meta:meta])*
        pub enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident
            ),+
            $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        pub enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )+
        }

        impl $name {
            /// Every inventory member in stable declaration order.
            pub const ALL: &'static [Self] = &[
                $(
                    Self::$variant,
                )+
            ];
        }
    };
}

pub(crate) use closed_inventory;
