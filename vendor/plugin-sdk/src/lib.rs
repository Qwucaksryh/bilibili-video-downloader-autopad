pub use bilibili_video_downloader_plugin_api::SDK_API_VERSION_V1 as SDK_API_VERSION;
pub use bilibili_video_downloader_plugin_api::v1::{
    AfterPreparePayloadV1, BeforeVideoProcessPayloadV1, CanvasConfigV1, DownloadProgressV1,
    FileExistActionV1, HookInputV1, HookOutputV1, HookPayloadV1, HookPointV1, HostApiV1,
    HostConfigV1, OnCompletedPayloadV1, PluginDescriptorV1, PluginFailurePolicy, ProxyModeV1,
};
pub use eyre;
pub use parking_lot;
pub use serde_json;

use std::sync::LazyLock;

use parking_lot::Mutex;

pub trait PluginV1: Default + Send + 'static {
    fn descriptor(&self) -> PluginDescriptorV1;
    #[allow(clippy::missing_errors_doc)]
    fn on_hook(&mut self, input: HookInputV1) -> eyre::Result<HookOutputV1>;
}

static HOST_API_V1: LazyLock<Mutex<Option<HostApiV1>>> = LazyLock::new(|| Mutex::new(None));

#[doc(hidden)]
pub unsafe fn register_host_api_v1(api_ptr: *const HostApiV1) -> i32 {
    if api_ptr.is_null() {
        return 1;
    }

    let api = unsafe { *api_ptr };
    *HOST_API_V1.lock() = Some(api);

    0
}

fn get_host_api_v1() -> eyre::Result<HostApiV1> {
    HOST_API_V1
        .lock()
        .as_ref()
        .copied()
        .ok_or_else(|| eyre::eyre!("host api 未注册"))
}

pub mod host {
    use crate::HostConfigV1;

    #[allow(clippy::missing_errors_doc)]
    pub fn get_config() -> eyre::Result<HostConfigV1> {
        let host_api = crate::get_host_api_v1()?;

        let mut output_ptr: *mut u8 = std::ptr::null_mut();
        let mut output_len: usize = 0;
        let rc = unsafe { (host_api.get_config_json)(&raw mut output_ptr, &raw mut output_len) };
        if rc != 0 {
            return Err(eyre::eyre!("host get_config_json 调用失败: rc={rc}"));
        }

        if output_ptr.is_null() {
            return Err(eyre::eyre!("host get_config_json 返回的缓冲区为空指针"));
        }

        let output_bytes = unsafe { std::slice::from_raw_parts(output_ptr, output_len) }.to_vec();
        unsafe {
            (host_api.free_buffer)(output_ptr, output_len);
        }

        let host_config = serde_json::from_slice::<HostConfigV1>(&output_bytes)?;

        Ok(host_config)
    }
}

#[macro_export]
macro_rules! export_plugin_v1 {
    ($ty:ty) => {
        use std::ffi::{CString, c_char};
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::LazyLock;
        use $crate::parking_lot::Mutex;

        fn to_cstring_lossy(value: String) -> CString {
            // Replacing NUL guarantees CString invariants and avoids fallible construction.
            let sanitized = value.replace('\0', " ");
            unsafe { CString::from_vec_unchecked(sanitized.into_bytes()) }
        }

        static INSTANCE_V1: LazyLock<Mutex<$ty>> = LazyLock::new(|| Mutex::new(<$ty>::default()));
        static DESCRIPTOR_JSON_V1: LazyLock<CString> = LazyLock::new(|| {
            let instance = INSTANCE_V1.lock();
            let descriptor = instance.descriptor();
            let descriptor_json = match $crate::serde_json::to_string(&descriptor) {
                Ok(json) => json,
                Err(err) => format!("{{\"error\":\"序列化 descriptor 失败: {err}\"}}"),
            };
            to_cstring_lossy(descriptor_json)
        });
        static LAST_ERROR_V1: LazyLock<Mutex<CString>> =
            LazyLock::new(|| Mutex::new(to_cstring_lossy(String::new())));

        fn set_last_error_v1(message: String) {
            let mut guard = LAST_ERROR_V1.lock();
            *guard = to_cstring_lossy(message);
        }

        #[unsafe(export_name = "bilibili_video_downloader_plugin_descriptor_v1")]
        pub extern "C" fn descriptor_v1() -> *const c_char {
            DESCRIPTOR_JSON_V1.as_ptr()
        }

        #[unsafe(export_name = "bilibili_video_downloader_plugin_last_error_v1")]
        pub extern "C" fn last_error_v1() -> *const c_char {
            LAST_ERROR_V1.lock().as_ptr()
        }

        #[unsafe(export_name = "bilibili_video_downloader_plugin_set_host_api_v1")]
        pub unsafe extern "C" fn set_host_api_v1(api: *const $crate::HostApiV1) -> i32 {
            let rc = unsafe { $crate::register_host_api_v1(api) };
            if rc != 0 {
                set_last_error_v1("无效的 host api 指针".to_string());
            }
            rc
        }

        #[unsafe(export_name = "bilibili_video_downloader_plugin_on_hook_v1")]
        pub unsafe extern "C" fn on_hook_v1(
            input_ptr: *const u8,
            input_len: usize,
            out_ptr: *mut *mut u8,
            out_len: *mut usize,
        ) -> i32 {
            if input_ptr.is_null() || out_ptr.is_null() || out_len.is_null() {
                set_last_error_v1("参数里有空指针".to_string());
                return 1;
            }

            let input_slice = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
            let hook_input: $crate::HookInputV1 = match $crate::serde_json::from_slice(input_slice)
            {
                Ok(input) => input,
                Err(err) => {
                    set_last_error_v1(format!("解析 hook 输入失败: {err}"));
                    return 2;
                }
            };

            let hook_output = match catch_unwind(AssertUnwindSafe(|| {
                let mut plugin = INSTANCE_V1.lock();
                plugin.on_hook(hook_input)
            })) {
                Ok(Ok(output)) => output,
                Ok(Err(err)) => {
                    set_last_error_v1(format!("{err:?}"));
                    return 3;
                }
                Err(_) => {
                    set_last_error_v1("处理 on_hook 时插件内部发生 panic".to_string());
                    return 5;
                }
            };

            let output_bytes = match $crate::serde_json::to_vec(&hook_output) {
                Ok(bytes) => bytes,
                Err(err) => {
                    set_last_error_v1(format!("序列化 hook 输出失败: {err}"));
                    return 4;
                }
            };

            let boxed = output_bytes.into_boxed_slice();
            let len = boxed.len();
            let ptr = Box::into_raw(boxed) as *mut u8;

            unsafe {
                *out_ptr = ptr;
                *out_len = len;
            }

            0
        }

        #[unsafe(export_name = "bilibili_video_downloader_plugin_free_buffer_v1")]
        pub unsafe extern "C" fn free_buffer_v1(ptr: *mut u8, len: usize) {
            if ptr.is_null() || len == 0 {
                return;
            }
            let raw_slice = std::ptr::slice_from_raw_parts_mut(ptr, len);
            unsafe {
                drop(Box::from_raw(raw_slice));
            }
        }
    };
}
