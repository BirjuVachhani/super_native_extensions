use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

use jni::{
    objects::{GlobalRef, JObject},
    sys::{jbyte, jint},
    AttachGuard, JNIEnv,
};

use nativeshell_core::{util::FutureCompleter, Context, RunLoopSender, Value};
use once_cell::sync::OnceCell;
use url::Url;

use crate::{
    android::{CLIP_DATA_HELPER, CONTEXT, JAVA_VM},
    error::{NativeExtensionsError, NativeExtensionsResult},
    reader_manager::ReadProgress,
    util::DropNotifier,
};

use super::MIME_TYPE_URI_LIST;

pub struct PlatformDataReader {
    clip_data: Option<GlobalRef>,
    // If needed enhance life of local data source
    _source_drop_notifier: Option<Arc<DropNotifier>>,
}

static RUN_LOOP_SENDER: OnceCell<RunLoopSender> = OnceCell::new();

impl PlatformDataReader {
    fn get_env_and_context() -> NativeExtensionsResult<(AttachGuard<'static>, JObject<'static>)> {
        let env = JAVA_VM
            .get()
            .ok_or_else(|| NativeExtensionsError::OtherError("JAVA_VM not set".into()))?
            .attach_current_thread()?;
        let context = CONTEXT.get().unwrap().as_obj();
        Ok((env, context))
    }

    pub fn get_items_sync(&self) -> NativeExtensionsResult<Vec<i64>> {
        match &self.clip_data {
            Some(clip_data) => {
                let (env, _) = Self::get_env_and_context()?;
                let count = env
                    .call_method(clip_data.as_obj(), "getItemCount", "()I", &[])?
                    .i()?;
                Ok((0..count as i64).collect())
            }
            None => Ok(Vec::new()),
        }
    }

    pub async fn get_items(&self) -> NativeExtensionsResult<Vec<i64>> {
        self.get_items_sync()
    }

    pub fn get_formats_for_item_sync(&self, item: i64) -> NativeExtensionsResult<Vec<String>> {
        match &self.clip_data {
            Some(clip_data) => {
                let (env, context) = Self::get_env_and_context()?;
                let formats = env
                    .call_method(
                        CLIP_DATA_HELPER.get().unwrap().as_obj(),
                        "getFormats",
                        "(Landroid/content/ClipData;ILandroid/content/Context;)[Ljava/lang/String;",
                        &[clip_data.as_obj().into(), item.into(), context.into()],
                    )?
                    .l()?;
                if formats.is_null() {
                    Ok(Vec::new())
                } else {
                    (0..env.get_array_length(*formats)?)
                        .map(|i| {
                            let obj = env.get_object_array_element(*formats, i)?;
                            Ok(env.get_string(obj.into())?.into())
                        })
                        .collect()
                }
            }
            None => Ok(Vec::new()),
        }
    }

    pub async fn get_suggested_name_for_item(
        &self,
        item: i64,
    ) -> NativeExtensionsResult<Option<String>> {
        let formats = self.get_formats_for_item_sync(item)?;
        if formats.iter().any(|s| s == MIME_TYPE_URI_LIST) {
            let uri = self
                .get_data_for_item(item, MIME_TYPE_URI_LIST.to_owned(), None)
                .await?;
            if let Value::String(url) = uri {
                if let Ok(url) = Url::parse(&url) {
                    if let Some(segments) = url.path_segments() {
                        let last: Option<&str> = segments.last().filter(|s| !s.is_empty());
                        return Ok(last.map(|f| f.to_owned()));
                    }
                }
            }
        }
        Ok(None)
    }

    pub async fn get_formats_for_item(&self, item: i64) -> NativeExtensionsResult<Vec<String>> {
        self.get_formats_for_item_sync(item)
    }

    thread_local! {
        static NEXT_HANDLE: Cell<i64> = Cell::new(1);
        static PENDING:
            RefCell<HashMap<i64,FutureCompleter<NativeExtensionsResult<Value>>>> = RefCell::new(HashMap::new());
    }

    #[no_mangle]
    #[allow(non_snake_case)]
    pub extern "C" fn Java_com_superlist_super_1native_1extensions_ClipDataHelper_onData(
        env: jni::JNIEnv,
        _class: jni::objects::JClass,
        handle: jint,
        data: jni::objects::JObject,
    ) {
        let sender = RUN_LOOP_SENDER.get().unwrap();
        unsafe fn transform_slice_mut<T>(s: &mut [T]) -> &mut [jbyte] {
            std::slice::from_raw_parts_mut(
                s.as_mut_ptr() as *mut jbyte,
                s.len() * std::mem::size_of::<T>(),
            )
        }
        let data = move || {
            if data.is_null() {
                Ok(Value::Null)
            } else if env.is_instance_of(data, "java/lang/CharSequence")? {
                Ok(Value::String(env.get_string(data.into())?.into()))
            } else {
                let mut res = Vec::new();
                res.resize(env.get_array_length(*data)? as usize, 0);
                env.get_byte_array_region(*data, 0, unsafe { transform_slice_mut(&mut res) })?;
                Ok(Value::U8List(res))
            }
        };
        let result: Result<Value, NativeExtensionsError> = data();

        sender.send(move || {
            let completer = Self::PENDING.with(|m| m.borrow_mut().remove(&(handle as i64)));
            if let Some(completer) = completer {
                completer.complete(result);
            }
        });
    }

    pub async fn get_data_for_item(
        &self,
        item: i64,
        format: String,
        _progress: Option<Arc<ReadProgress>>,
    ) -> NativeExtensionsResult<Value> {
        RUN_LOOP_SENDER.get_or_init(|| Context::get().run_loop().new_sender());
        match &self.clip_data {
            Some(clip_data) => {
                let (future, completer) = FutureCompleter::new();
                let (env, context) = Self::get_env_and_context()?;

                let handle = Self::NEXT_HANDLE.with(|h| {
                    let res = h.get();
                    h.set(res + 1);
                    res
                });
                Self::PENDING.with(|m| m.borrow_mut().insert(handle, completer));

                env.call_method(
                    CLIP_DATA_HELPER.get().unwrap().as_obj(),
                    "getData",
                    "(Landroid/content/ClipData;ILjava/lang/String;Landroid/content/Context;I)V",
                    &[
                        clip_data.as_obj().into(),
                        item.into(),
                        env.new_string(format)?.into(),
                        context.into(),
                        handle.into(),
                    ],
                )?;

                future.await
            }
            None => Ok(Value::Null),
        }
    }

    pub fn from_clip_data<'a>(
        env: &JNIEnv<'a>,
        clip_data: JObject<'a>,
        source_drop_notifier: Option<Arc<DropNotifier>>,
    ) -> NativeExtensionsResult<Rc<Self>> {
        let clip_data = if clip_data.is_null() {
            None
        } else {
            Some(env.new_global_ref(clip_data)?)
        };
        Ok(Rc::new(Self {
            clip_data,
            _source_drop_notifier: source_drop_notifier,
        }))
    }

    pub fn new_clipboard_reader() -> NativeExtensionsResult<Rc<Self>> {
        let (env, context) = Self::get_env_and_context()?;
        let clipboard_service = env
            .get_static_field(
                "android/content/Context",
                "CLIPBOARD_SERVICE",
                "Ljava/lang/String;",
            )?
            .l()?;
        let clipboard_manager = env
            .call_method(
                context,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[clipboard_service.into()],
            )?
            .l()?;
        let clip_data = env
            .call_method(
                clipboard_manager,
                "getPrimaryClip",
                "()Landroid/content/ClipData;",
                &[],
            )?
            .l()?;
        Self::from_clip_data(&env, clip_data, None)
    }

    pub fn item_format_is_synthetized(
        &self,
        _item: i64,
        _format: &str,
    ) -> NativeExtensionsResult<bool> {
        Ok(false)
    }

    pub async fn can_get_virtual_file_for_item(
        &self,
        _item: i64,
        _format: &str,
    ) -> NativeExtensionsResult<bool> {
        Ok(false)
    }

    pub async fn get_virtual_file_for_item(
        &self,
        _item: i64,
        _format: &str,
        _target_folder: PathBuf,
        _progress: Arc<ReadProgress>,
    ) -> NativeExtensionsResult<PathBuf> {
        Err(NativeExtensionsError::UnsupportedOperation)
    }
}