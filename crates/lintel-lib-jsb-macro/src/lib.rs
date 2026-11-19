use std::fmt::Debug;
use tokio::sync::oneshot;

/// 展开时依赖的定义方路径，不要求调用方直接声明辅助依赖。
#[doc(hidden)]
pub mod __private {
  pub use paste;
  pub use tracing;
}

#[must_use]
pub struct ResponseTx<T> {
  context_id: String,
  resp_tx: oneshot::Sender<anyhow::Result<T>>,
}

impl<T> ResponseTx<T> {
  pub fn new(context_id: String, resp_tx: oneshot::Sender<anyhow::Result<T>>) -> Self {
    Self { context_id, resp_tx }
  }

  pub fn execute<F>(self, func: F)
  where
    F: FnOnce() -> anyhow::Result<T>,
  {
    self.send(func());
  }

  pub fn send(self, result: anyhow::Result<T>) {
    if self.resp_tx.send(result).is_err() {
      tracing::error!(context_id = %self.context_id, "failed to send execution result");
    }
  }
}

impl<T> Debug for ResponseTx<T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ResponseTx")
      .field("context_id", &self.context_id)
      .finish()
  }
}

#[macro_export]
macro_rules! define_api_method {
    // Case with return type
    ($variant:ident, $sender_field:ident, $enum_name:ident,
     [ $( $param_name:ident: $param_type:ty ),* ],
     [ $return_type:ty ]) => {
        $crate::__private::paste::paste! {
            pub fn [<$variant:snake>](
                &self,
                $( $param_name: $param_type, )*
                resp_tx: $crate::ResponseTx<$return_type>
            ) {
                if let Err(_error) = self.$sender_field.send(
                    $enum_name::$variant(
                        $( $param_name, )*
                        resp_tx
                    )
                ) {
                    $crate::__private::tracing::error!(
                        concat!("failed to send ", stringify!($enum_name), "::", stringify!($variant))
                    );
                }
            }
        }
    };

    // Case without return type
    ($variant:ident, $sender_field:ident, $enum_name:ident,
     [ $( $param_name:ident: $param_type:ty ),* ],
     [ ]) => {
        $crate::__private::paste::paste! {
            pub fn [<$variant:snake>](
                &self,
                $( $param_name: $param_type, )*
            ) {
                if let Err(_error) = self.$sender_field.send(
                    $enum_name::$variant(
                        $( $param_name, )*
                    )
                ) {
                    $crate::__private::tracing::error!(
                        concat!("failed to send ", stringify!($enum_name), "::", stringify!($variant))
                    );
                }
            }
        }
    };
}

#[macro_export]
macro_rules! define_api {
    (
        enum $enum_name:ident {
            $(
                $variant:ident $(( $($param_name:ident: $param_type:ty),* ))? $(-> $return_type:ty)? ,
            )*
        }

        struct $api_struct:ident {
            $sender_field:ident: $sender_type:ty,
        }
    ) => {
        #[derive(Debug)]
        enum $enum_name {
            $(
                $variant(
                    $( $( $param_type, )* )?
                    $($crate::ResponseTx<$return_type>)?
                ),
            )*
        }

        #[derive(Clone, Debug)]
        pub struct $api_struct {
            $sender_field: $sender_type,
        }

        impl $api_struct {
            pub fn new($sender_field: $sender_type) -> Self {
                Self { $sender_field }
            }

            $(
                $crate::define_api_method!{
                    $variant,
                    $sender_field,
                    $enum_name,
                    [ $( $( $param_name: $param_type ),* )? ],
                    [ $( $return_type )? ]
                }
            )*
        }
    };
}
