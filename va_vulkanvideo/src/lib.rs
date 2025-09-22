use std::{
    borrow::Cow,
    ffi::{CStr, c_float, c_int, c_short, c_uchar, c_uint, c_ulong, c_ushort, c_void},
    fmt,
    fs::File,
    os::{
        fd::{FromRawFd, IntoRawFd, RawFd},
        linux::fs::MetadataExt,
        unix::fs::FileTypeExt,
    },
};

use ash::{
    ext, khr,
    prelude::*,
    vk::{self, native},
};
use log::{debug, error, info, trace, warn};
use simple_logger::SimpleLogger;

use va_backend_sys::{
    VA_STATUS_SUCCESS, VABufferID, VABufferType, VAConfigAttrib, VAConfigID, VAContextID,
    VADisplayAttribute, VADriverContext, VADriverContextP, VADriverInit, VADriverVTable,
    VAEntrypoint, VAImage, VAImageFormat, VAImageID, VAProfile, VAStatus, VASubpictureID,
    VASurfaceID, VASurfaceStatus, drm_state,
};

fn with_driver_context(
    driver_context: VADriverContextP,
    f: impl FnOnce(&mut VADriverContext) -> Result<(), VaError>,
) -> VAStatus {
    let driver_context = unsafe { driver_context_as_ref(driver_context) };
    let driver_context = match driver_context {
        Ok(ctx) => ctx,
        Err(e) => return e.into(),
    };
    f(driver_context)
        .map(|_| VA_STATUS_SUCCESS as VAStatus)
        .unwrap_or_else(|e| e.into())
}

extern "C" fn va_terminate(driver_context: VADriverContextP) -> VAStatus {
    with_driver_context(driver_context, |driver_context| {
        let driver_data = std::mem::take(&mut driver_context.pDriverData);
        if !driver_data.is_null() {
            unsafe {
                // Reconstruct the Box and drop it
                let _boxed: Box<DriverData> = Box::from_raw(driver_data as *mut DriverData);
            }
        } else {
            warn!("Driver data pointer is null on terminate");
        }
        Ok(())
    })
}

extern "C" fn va_query_config_profiles(
    driver_context: VADriverContextP,
    profile_list: *mut VAProfile, // out
    num_profiles: *mut c_int,     // out
) -> VAStatus {
    if profile_list.is_null() || !profile_list.is_aligned() {
        return VaError::InvalidParameter.into();
    }
    if num_profiles.is_null() || !num_profiles.is_aligned() {
        return VaError::InvalidParameter.into();
    }

    with_driver_context(driver_context, |driver_context| {
        let driver_data = unsafe { DriverData::from_ptr(driver_context.pDriverData)? };

        let codecs = &driver_data.vulkan.supported_codecs;
        let mut supported_profiles = Vec::new();

        // TODO: Does this suffice?
        if codecs.h264_decode || codecs.h264_encode {
            // `Baseline` is deprecated and equivalent to `Constrained Baseline`
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileH264ConstrainedBaseline);
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileH264Main);
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileH264High);
        }
        if codecs.h265_decode || codecs.h265_encode {
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileHEVCMain);
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileHEVCMain10);
        }
        if codecs.av1_decode || codecs.av1_encode {
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileAV1Profile0);
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileAV1Profile1);
        }
        if codecs.vp9_decode {
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileVP9Profile0);
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileVP9Profile1);
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileVP9Profile2);
            supported_profiles.push(va_backend_sys::VAProfile_VAProfileVP9Profile3);
        }

        if supported_profiles.len() > driver_context.max_profiles as usize {
            // Should never happen, max_profiles is normally only set by us
            return Err(VaError::OperationFailed);
        }

        // SAFETY: Null/unaligned checks are done above. Docs state:
        // > The caller must provide a "profile_list" array that can hold at least
        // > vaMaxNumProfile() entries.
        unsafe {
            profile_list
                .copy_from_nonoverlapping(supported_profiles.as_ptr(), supported_profiles.len());
            *num_profiles = supported_profiles.len() as c_int;
        }

        Ok(())
    })
}

const MAX_ENTRYPOINTS: usize = 2; // Decode and Encode

extern "C" fn va_query_config_entrypoints(
    driver_context: VADriverContextP,
    profile: VAProfile,
    entrypoint_list: *mut VAEntrypoint, // out
    num_entrypoints: *mut c_int,        // out
) -> VAStatus {
    with_driver_context(driver_context, |driver_context| {
        let driver_data = unsafe { DriverData::from_ptr(driver_context.pDriverData)? };
        let (decode, encode) = match profile {
            va_backend_sys::VAProfile_VAProfileH264Baseline
            | va_backend_sys::VAProfile_VAProfileH264ConstrainedBaseline
            | va_backend_sys::VAProfile_VAProfileH264Main
            | va_backend_sys::VAProfile_VAProfileH264High => (
                driver_data.vulkan.supported_codecs.h264_decode,
                driver_data.vulkan.supported_codecs.h264_encode,
            ),
            va_backend_sys::VAProfile_VAProfileHEVCMain
            | va_backend_sys::VAProfile_VAProfileHEVCMain10 => (
                driver_data.vulkan.supported_codecs.h265_decode,
                driver_data.vulkan.supported_codecs.h265_encode,
            ),
            va_backend_sys::VAProfile_VAProfileAV1Profile0
            | va_backend_sys::VAProfile_VAProfileAV1Profile1 => (
                driver_data.vulkan.supported_codecs.av1_decode,
                driver_data.vulkan.supported_codecs.av1_encode,
            ),
            va_backend_sys::VAProfile_VAProfileVP9Profile0
            | va_backend_sys::VAProfile_VAProfileVP9Profile1
            | va_backend_sys::VAProfile_VAProfileVP9Profile2
            | va_backend_sys::VAProfile_VAProfileVP9Profile3 => (
                driver_data.vulkan.supported_codecs.vp9_decode,
                false, // No VP9 encode support
            ),
            _ => return Err(VaError::UnsupportedProfile),
        };

        if MAX_ENTRYPOINTS > driver_context.max_entrypoints as usize {
            // Should never happen, max_entrypoints is normally only set by us
            return Err(VaError::OperationFailed);
        }

        let entry_points = [
            va_backend_sys::VAEntrypoint_VAEntrypointVLD,
            va_backend_sys::VAEntrypoint_VAEntrypointEncSlice,
        ];
        let range = if decode && encode {
            0..2
        } else if decode {
            0..1
        } else if encode {
            1..2
        } else {
            // Shouldn't happen, as the profile wouldn't be listed in the first place
            return Err(VaError::UnsupportedProfile);
        };
        let entry_points = &entry_points[range];

        // SAFETY: Null/unaligned checks are done above. Docs state:
        // > The caller must provide a "profile_list" array that can hold at least
        // > vaMaxNumProfile() entries.
        unsafe {
            entrypoint_list.copy_from_nonoverlapping(entry_points.as_ptr(), entry_points.len());
            *num_entrypoints = entry_points.len() as c_int;
        }

        Ok(())
    })
}

extern "C" fn va_create_config(
    driver_context: VADriverContextP,
    _profile: VAProfile,
    _entrypoint: VAEntrypoint,
    _attrib_list: *mut VAConfigAttrib,
    _num_attribs: c_int,
    _config_id: *mut VAConfigID, // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_destroy_config(
    driver_context: VADriverContextP,
    _config_id: VAConfigID,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_get_config_attributes(
    driver_context: VADriverContextP,
    _profile: VAProfile,
    _entrypoint: VAEntrypoint,
    _attrib_list: *mut VAConfigAttrib, // in/out
    _num_attribs: c_int,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_query_config_attributes(
    driver_context: VADriverContextP,
    _config_id: VAConfigID,
    _profile: *mut VAProfile,          // out
    _entrypoint: *mut VAEntrypoint,    // out
    _attrib_list: *mut VAConfigAttrib, // out
    _num_attribs: *mut c_int,          // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_create_surfaces(
    driver_context: VADriverContextP,
    _width: c_int,
    _height: c_int,
    _format: c_int,
    _num_surfaces: c_int,
    _surfaces: *mut VASurfaceID, // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_destroy_surfaces(
    driver_context: VADriverContextP,
    _surface_list: *mut VASurfaceID,
    _num_surfaces: c_int,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_create_context(
    driver_context: VADriverContextP,
    _config_id: VAConfigID,
    _picture_width: c_int,
    _picture_height: c_int,
    _flag: c_int,
    _render_targets: *mut VASurfaceID,
    _num_render_targets: c_int,
    _context: *mut VAContextID, // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_destroy_context(
    driver_context: VADriverContextP,
    _context: VAContextID,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_create_buffer(
    driver_context: VADriverContextP,
    _context: VAContextID, // in
    _type: VABufferType,   // in
    _size: c_uint,         // in
    _num_elements: c_uint, // in
    _data: *mut c_void,    // in
    _buf_id: *mut VABufferID,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_buffer_set_num_elements(
    driver_context: VADriverContextP,
    _buf_id: VABufferID,   // in
    _num_elements: c_uint, // in
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_map_buffer(
    driver_context: VADriverContextP,
    _buf_id: VABufferID,     // in
    _pbuf: *mut *mut c_void, // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_unmap_buffer(driver_context: VADriverContextP, _buf_id: VABufferID) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_destroy_buffer(
    driver_context: VADriverContextP,
    _buffer_id: VABufferID,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_begin_picture(
    driver_context: VADriverContextP,
    _context: VAContextID,
    _render_target: VASurfaceID,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_render_picture(
    driver_context: VADriverContextP,
    _context: VAContextID,
    _buffers: *mut VABufferID,
    _num_buffers: c_int,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_end_picture(driver_context: VADriverContextP, _context: VAContextID) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_sync_surface(
    driver_context: VADriverContextP,
    _render_target: VASurfaceID,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_query_surface_status(
    driver_context: VADriverContextP,
    _render_target: VASurfaceID,
    _status: *mut VASurfaceStatus, // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_query_image_formats(
    driver_context: VADriverContextP,
    _format_list: *mut VAImageFormat, // out
    _num_formats: *mut c_int,         // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_create_image(
    driver_context: VADriverContextP,
    _format: *mut VAImageFormat,
    _width: c_int,
    _height: c_int,
    _image: *mut VAImage, // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_derive_image(
    driver_context: VADriverContextP,
    _surface: VASurfaceID,
    _image: *mut VAImage, // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_destroy_image(driver_context: VADriverContextP, _image: VAImageID) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

/// palette:
/// > pointer to an array holding the palette data.  The size of the array is num_palette_entries *
/// > entry_bytes in size.  The order of the components in the palette is described by the
/// > component_order in VAImage struct
extern "C" fn va_set_image_palette(
    driver_context: VADriverContextP,
    _image: VAImageID,
    _palette: *mut c_uchar,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

/// x, y:
/// > coordinates of the upper left source pixel
///
/// width, height:
/// > width and height of the region
extern "C" fn va_get_image(
    driver_context: VADriverContextP,
    _surface: VASurfaceID,
    _x: c_int,
    _y: c_int,
    _width: c_uint,
    _height: c_uint,
    _image: VAImageID,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_put_image(
    driver_context: VADriverContextP,
    _surface: VASurfaceID,
    _image: VAImageID,
    _src_x: c_int,
    _src_y: c_int,
    _src_width: c_uint,
    _src_height: c_uint,
    _dest_x: c_int,
    _dest_y: c_int,
    _dest_width: c_uint,
    _dest_height: c_uint,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_query_subpicture_formats(
    driver_context: VADriverContextP,
    _format_list: *mut VAImageFormat, // out
    _flags: *mut c_uint,              // out
    _num_formats: *mut c_uint,        // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_create_subpicture(
    driver_context: VADriverContextP,
    _image: VAImageID,
    _subpicture: *mut VASubpictureID, // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_destroy_subpicture(
    driver_context: VADriverContextP,
    _subpicture: VASubpictureID,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_set_subpicture_image(
    driver_context: VADriverContextP,
    _subpicture: VASubpictureID,
    _image: VAImageID,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_set_subpicture_chromakey(
    driver_context: VADriverContextP,
    _subpicture: VASubpictureID,
    _chromakey_min: c_uint,
    _chromakey_max: c_uint,
    _chromakey_mask: c_uint,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_set_subpicture_global_alpha(
    driver_context: VADriverContextP,
    _subpicture: VASubpictureID,
    _global_alpha: c_float,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

/// src_x, src_y:
/// > upper left offset in subpicture
///
/// dest_x, dest_y:
/// > upper left offset in surface
///
/// flags:
/// > whether to enable chroma-keying or global-alpha
/// > see VA_SUBPICTURE_XXX values
extern "C" fn va_associate_subpicture(
    driver_context: VADriverContextP,
    _subpicture: VASubpictureID,
    _target_surfaces: *mut VASurfaceID,
    _num_surfaces: c_int,
    _src_x: c_short,
    _src_y: c_short,
    _src_width: c_ushort,
    _src_height: c_ushort,
    _dest_x: c_short,
    _dest_y: c_short,
    _dest_width: c_ushort,
    _dest_height: c_ushort,
    _flags: c_uint,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_deassociate_subpicture(
    driver_context: VADriverContextP,
    _subpicture: VASubpictureID,
    _target_surfaces: *mut VASurfaceID,
    _num_surfaces: c_int,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_query_display_attributes(
    driver_context: VADriverContextP,
    _attr_list: *mut VADisplayAttribute, // out
    _num_attributes: *mut c_int,         // out
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_get_display_attributes(
    driver_context: VADriverContextP,
    _attr_list: *mut VADisplayAttribute, // in/out
    _num_attributes: c_int,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

extern "C" fn va_set_display_attributes(
    driver_context: VADriverContextP,
    _attr_list: *mut VADisplayAttribute,
    _num_attributes: c_int,
) -> VAStatus {
    with_driver_context(driver_context, |_driver_context| {
        Err(VaError::Unimplemented)
    })
}

fn fill_vtable(vtable: &mut VADriverVTable) {
    *vtable = VADriverVTable {
        vaTerminate: Some(va_terminate),
        vaQueryConfigProfiles: Some(va_query_config_profiles),
        vaQueryConfigEntrypoints: Some(va_query_config_entrypoints),
        vaGetConfigAttributes: Some(va_get_config_attributes),
        vaCreateConfig: Some(va_create_config),
        vaDestroyConfig: Some(va_destroy_config),
        vaQueryConfigAttributes: Some(va_query_config_attributes),
        vaCreateSurfaces: Some(va_create_surfaces),
        vaDestroySurfaces: Some(va_destroy_surfaces),
        vaCreateContext: Some(va_create_context),
        vaDestroyContext: Some(va_destroy_context),
        vaCreateBuffer: Some(va_create_buffer),
        vaBufferSetNumElements: Some(va_buffer_set_num_elements),
        vaMapBuffer: Some(va_map_buffer),
        vaUnmapBuffer: Some(va_unmap_buffer),
        vaDestroyBuffer: Some(va_destroy_buffer),
        vaBeginPicture: Some(va_begin_picture),
        vaRenderPicture: Some(va_render_picture),
        vaEndPicture: Some(va_end_picture),
        vaSyncSurface: Some(va_sync_surface),
        vaQuerySurfaceStatus: Some(va_query_surface_status),
        vaQuerySurfaceError: None, // TODO:
        vaPutSurface: None,        // TODO:
        vaQueryImageFormats: Some(va_query_image_formats),
        vaCreateImage: Some(va_create_image),
        vaDeriveImage: Some(va_derive_image),
        vaDestroyImage: Some(va_destroy_image),
        vaSetImagePalette: Some(va_set_image_palette),
        vaGetImage: Some(va_get_image),
        vaPutImage: Some(va_put_image),
        vaQuerySubpictureFormats: Some(va_query_subpicture_formats),
        vaCreateSubpicture: Some(va_create_subpicture),
        vaDestroySubpicture: Some(va_destroy_subpicture),
        vaSetSubpictureImage: Some(va_set_subpicture_image),
        vaSetSubpictureChromakey: Some(va_set_subpicture_chromakey),
        vaSetSubpictureGlobalAlpha: Some(va_set_subpicture_global_alpha),
        vaAssociateSubpicture: Some(va_associate_subpicture),
        vaDeassociateSubpicture: Some(va_deassociate_subpicture),
        vaQueryDisplayAttributes: Some(va_query_display_attributes),
        vaGetDisplayAttributes: Some(va_get_display_attributes),
        vaSetDisplayAttributes: Some(va_set_display_attributes),
        vaBufferInfo: None,             // TODO:
        vaLockSurface: None,            // TODO:
        vaUnlockSurface: None,          // TODO:
        vaGetSurfaceAttributes: None,   // TODO:
        vaCreateSurfaces2: None,        // TODO:
        vaQuerySurfaceAttributes: None, // TODO:
        vaAcquireBufferHandle: None,    // TODO:
        vaReleaseBufferHandle: None,    // TODO:
        vaCreateMFContext: None,        // TODO:
        vaMFAddContext: None,           // TODO:
        vaMFReleaseContext: None,       // TODO:
        vaMFSubmit: None,               // TODO:
        vaCreateBuffer2: None,          // TODO:
        vaQueryProcessingRate: None,    // TODO:
        vaExportSurfaceHandle: None,    // TODO:
        vaSyncSurface2: None,           // TODO:
        vaSyncBuffer: None,             // TODO:
        vaCopy: None,                   // TODO:
        vaMapBuffer2: None,             // TODO:
        reserved: [0 as c_ulong; _],
    };
}

const VENDOR: &CStr = c"va_vulkan_video";

unsafe extern "system" fn vulkan_debug_callback(
    message_severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    message_type: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user_data: *mut std::os::raw::c_void,
) -> vk::Bool32 {
    let callback_data = unsafe { *p_callback_data };
    let message_id_number = callback_data.message_id_number;

    let message_id_name = if callback_data.p_message_id_name.is_null() {
        Cow::from("")
    } else {
        unsafe { CStr::from_ptr(callback_data.p_message_id_name) }.to_string_lossy()
    };

    let message = if callback_data.p_message.is_null() {
        Cow::from("")
    } else {
        unsafe { CStr::from_ptr(callback_data.p_message) }.to_string_lossy()
    };

    let level = match message_severity {
        vk::DebugUtilsMessageSeverityFlagsEXT::VERBOSE => log::Level::Trace,
        vk::DebugUtilsMessageSeverityFlagsEXT::INFO => log::Level::Info,
        vk::DebugUtilsMessageSeverityFlagsEXT::WARNING => log::Level::Warn,
        vk::DebugUtilsMessageSeverityFlagsEXT::ERROR => log::Level::Error,
        _ => log::Level::Info,
    };

    log::log!(
        level,
        "{message_type:?} [{message_id_name} ({message_id_number})] : {message}"
    );

    vk::FALSE
}

fn vulkan_device_is_same_as_drm(
    drm_properties: &vk::PhysicalDeviceDrmPropertiesEXT,
    drm_device_id: DeviceId,
) -> bool {
    let has_primary = drm_properties.has_primary == vk::TRUE;
    let has_render = drm_properties.has_render == vk::TRUE;

    let primary_id = DeviceId(drm_properties.primary_major, drm_properties.primary_minor);
    let render_id = DeviceId(drm_properties.render_major, drm_properties.render_minor);

    (has_primary && primary_id == drm_device_id) || (has_render && render_id == drm_device_id)
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Codec {
    H264,
    H265,
    Vp9,
    Av1,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Operation {
    Decode,
    Encode,
}

#[derive(Debug, Default)]
struct SupportedCodecs {
    // TODO: bitflags
    h264_decode: bool,
    h265_decode: bool,
    vp9_decode: bool,
    av1_decode: bool,
    h264_encode: bool,
    h265_encode: bool,
    av1_encode: bool,
}

struct CodecQueueFamilyInfo {
    index: usize,
    count: u32,
    operations: vk::VideoCodecOperationFlagsKHR,
    query_result_status_support: bool,
}

struct VulkanData {
    entry: ash::Entry,
    instance: ash::Instance,
    debug_utils_loader: ext::debug_utils::Instance,
    debug_call_back: vk::DebugUtilsMessengerEXT,
    physical_device: vk::PhysicalDevice,
    supported_codecs: SupportedCodecs,
    decode_queue_family: CodecQueueFamilyInfo,
}

// NOTE: Must be sorted by the extension name for binary search
const CODEC_EXTENSIONS: [(&CStr, Codec, Operation); 5] = [
    (khr::video_decode_av1::NAME, Codec::Av1, Operation::Decode),
    (khr::video_decode_h264::NAME, Codec::H264, Operation::Decode),
    (khr::video_decode_h265::NAME, Codec::H265, Operation::Decode),
    // (khr::video_decode_vp9::NAME, Codec::Vp9, Operation::Decode),
    // (khr::video_encode_av1::NAME, Codec::Av1, Operation::Encode),
    (khr::video_encode_h264::NAME, Codec::H264, Operation::Encode),
    (khr::video_encode_h265::NAME, Codec::H265, Operation::Encode),
];

fn init_vulkan(device_id: DeviceId) -> VkResult<VulkanData> {
    let entry = ash::Entry::linked();

    let app_info = vk::ApplicationInfo::default()
        .application_name(c"Vulkan Video VA-API Driver")
        .application_version(0)
        .engine_name(VENDOR)
        .engine_version(0)
        .api_version(vk::API_VERSION_1_3);

    let layer_names = vec![c"VK_LAYER_KHRONOS_validation".as_ptr()];
    let extension_names = vec![ext::debug_utils::NAME.as_ptr()];

    let mut debug_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(
            vk::DebugUtilsMessageSeverityFlagsEXT::VERBOSE
                | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                | vk::DebugUtilsMessageSeverityFlagsEXT::INFO,
        )
        .message_type(
            vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
        )
        .pfn_user_callback(Some(vulkan_debug_callback));

    let create_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_layer_names(&layer_names)
        .enabled_extension_names(&extension_names)
        .push_next(&mut debug_info);

    let instance = unsafe { entry.create_instance(&create_info, None)? };
    debug!("Vulkan instance created successfully");

    let debug_utils_loader = ext::debug_utils::Instance::new(&entry, &instance);
    let debug_call_back =
        unsafe { debug_utils_loader.create_debug_utils_messenger(&debug_info, None)? };
    debug!("Debug utils messenger created successfully");

    let physical_devices = unsafe { instance.enumerate_physical_devices()? };
    debug!("Found {} physical devices", physical_devices.len());

    // Try to select the physical device associated with the display
    // This uses similar logic as
    // https://wgpu.rs/doc/wgpu_hal/vulkan/struct.Instance.html#method.create_surface_from_drm

    let mut physical_device = None;

    // let video_queue_loader = khr::video_queue::Instance::new(&entry, &instance);

    for device in physical_devices {
        let mut drm_props = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut properties2 = vk::PhysicalDeviceProperties2KHR::default().push_next(&mut drm_props);
        unsafe {
            instance.get_physical_device_properties2(device, &mut properties2);
        }

        let properties = properties2.properties;

        // NOTE: This doesn't need the 2 variant (i.e. with pNext), but we might need it later
        // let mut features2 = vk::PhysicalDeviceFeatures2::default();
        // unsafe {
        //     instance.get_physical_device_features2(device, &mut features2);
        // }
        //
        // let features = features2.features;

        let extensions = unsafe { instance.enumerate_device_extension_properties(device)? };

        let mut supported_codecs = SupportedCodecs::default();
        for ext in extensions {
            let Ok(ext_name) = ext.extension_name_as_c_str() else {
                trace!("Invalid extension name: {:?}", ext.extension_name);
                continue;
            };

            let codec_ext = CODEC_EXTENSIONS.binary_search_by_key(&ext_name, |(name, _, _)| *name);
            if let Ok(i) = codec_ext {
                let (_, codec, operation) = CODEC_EXTENSIONS[i];
                match (codec, operation) {
                    (Codec::Av1, Operation::Decode) => supported_codecs.av1_decode = true,
                    (Codec::Av1, Operation::Encode) => supported_codecs.av1_encode = true,
                    (Codec::H264, Operation::Decode) => supported_codecs.h264_decode = true,
                    (Codec::H264, Operation::Encode) => supported_codecs.h264_encode = true,
                    (Codec::H265, Operation::Decode) => supported_codecs.h265_decode = true,
                    (Codec::H265, Operation::Encode) => supported_codecs.h265_encode = true,
                    (Codec::Vp9, Operation::Decode) => supported_codecs.vp9_decode = true,
                    (Codec::Vp9, Operation::Encode) => unimplemented!("VP9 encode"),
                }
            }
        }

        debug!("Supported codecs: {:?}", supported_codecs);

        if vulkan_device_is_same_as_drm(&drm_props, device_id) {
            info!(
                "Selected physical device: {} (ID: {:04x}:{:04x}, major/minor: {}/{})",
                unsafe { CStr::from_ptr(properties.device_name.as_ptr()).to_string_lossy() },
                properties.vendor_id,
                properties.device_id,
                device_id.0,
                device_id.1
            );
            physical_device = Some((device, supported_codecs));
            break;
        }
    }

    let Some((physical_device, supported_codecs)) = physical_device else {
        error!(
            "No suitable physical device found matching the DRM device ID {}/{}",
            device_id.0, device_id.1
        );
        return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
    };

    let queue_family_properties_len =
        unsafe { instance.get_physical_device_queue_family_properties2_len(physical_device) };
    debug!("Physical device has {queue_family_properties_len} queue families");

    let mut queue_family_video_properties =
        vec![vk::QueueFamilyVideoPropertiesKHR::default(); queue_family_properties_len];
    let mut queue_family_query_result_status_properties =
        vec![vk::QueueFamilyQueryResultStatusPropertiesKHR::default(); queue_family_properties_len];

    let mut queue_family_properties = queue_family_video_properties
        .iter_mut()
        .zip(queue_family_query_result_status_properties.iter_mut())
        .map(|(qfvp, qfrsp)| {
            vk::QueueFamilyProperties2KHR::default()
                .push_next(qfvp)
                .push_next(qfrsp)
        })
        .collect::<Vec<_>>();

    unsafe {
        instance.get_physical_device_queue_family_properties2(
            physical_device,
            &mut queue_family_properties,
        )
    };

    // Extract the inner `vk::QueueFamilyProperties` structs. This avoids issue with the mutable
    // borrow the outer struct holds on the other two structs via pNext.
    let queue_family_properties = queue_family_properties
        .into_iter()
        .map(|qfp| qfp.queue_family_properties)
        .collect::<Vec<_>>();

    // TODO: Improve selection logic, support multiple queue families, etc.
    let mut video_decode_qf = None;

    for i in 0..queue_family_properties.len() {
        let qfp = &queue_family_properties[i];
        let qfvp = &queue_family_video_properties[i];
        let qfrsp = &queue_family_query_result_status_properties[i];

        let query_result_status_support = qfrsp.query_result_status_support == vk::TRUE;

        debug!(
            "Queue family {i}: \
            flags={:?}, count={}, timestamp_valid_bits={}, \
            video_codec_operations={:?}, query_result_status_support={:?}",
            qfp.queue_flags,
            qfp.queue_count,
            qfp.timestamp_valid_bits,
            qfvp.video_codec_operations,
            query_result_status_support,
        );

        if qfp.queue_count > 0
            && qfp
                .queue_flags
                .contains(vk::QueueFlags::VIDEO_DECODE_KHR | vk::QueueFlags::TRANSFER)
        {
            video_decode_qf = Some(CodecQueueFamilyInfo {
                index: i,
                count: qfp.queue_count,
                operations: qfvp.video_codec_operations,
                query_result_status_support,
            });
        }
    }

    let Some(decode_queue_family) = video_decode_qf else {
        error!("No suitable video decode queue family found");
        return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
    };

    info!(
        "Selected video decode queue family {}",
        decode_queue_family.index,
    );

    Ok(VulkanData {
        entry,
        instance,
        debug_utils_loader,
        debug_call_back,
        physical_device,
        supported_codecs,
        decode_queue_family,
    })
}

impl Drop for VulkanData {
    fn drop(&mut self) {
        unsafe {
            self.debug_utils_loader
                .destroy_debug_utils_messenger(self.debug_call_back, None);
            self.instance.destroy_instance(None);
        }
    }
}

const PROFILES: [VAProfile; 39] = [
    va_backend_sys::VAProfile_VAProfileNone,
    va_backend_sys::VAProfile_VAProfileMPEG2Simple,
    va_backend_sys::VAProfile_VAProfileMPEG2Main,
    va_backend_sys::VAProfile_VAProfileMPEG4Simple,
    va_backend_sys::VAProfile_VAProfileMPEG4AdvancedSimple,
    va_backend_sys::VAProfile_VAProfileMPEG4Main,
    va_backend_sys::VAProfile_VAProfileH264Main,
    va_backend_sys::VAProfile_VAProfileH264High,
    va_backend_sys::VAProfile_VAProfileVC1Simple,
    va_backend_sys::VAProfile_VAProfileVC1Main,
    va_backend_sys::VAProfile_VAProfileVC1Advanced,
    va_backend_sys::VAProfile_VAProfileH263Baseline,
    va_backend_sys::VAProfile_VAProfileJPEGBaseline,
    va_backend_sys::VAProfile_VAProfileH264ConstrainedBaseline,
    va_backend_sys::VAProfile_VAProfileVP8Version0_3,
    va_backend_sys::VAProfile_VAProfileH264MultiviewHigh,
    va_backend_sys::VAProfile_VAProfileH264StereoHigh,
    va_backend_sys::VAProfile_VAProfileHEVCMain,
    va_backend_sys::VAProfile_VAProfileHEVCMain10,
    va_backend_sys::VAProfile_VAProfileVP9Profile0,
    va_backend_sys::VAProfile_VAProfileVP9Profile1,
    va_backend_sys::VAProfile_VAProfileVP9Profile2,
    va_backend_sys::VAProfile_VAProfileVP9Profile3,
    va_backend_sys::VAProfile_VAProfileHEVCMain12,
    va_backend_sys::VAProfile_VAProfileHEVCMain422_10,
    va_backend_sys::VAProfile_VAProfileHEVCMain422_12,
    va_backend_sys::VAProfile_VAProfileHEVCMain444,
    va_backend_sys::VAProfile_VAProfileHEVCMain444_10,
    va_backend_sys::VAProfile_VAProfileHEVCMain444_12,
    va_backend_sys::VAProfile_VAProfileHEVCSccMain,
    va_backend_sys::VAProfile_VAProfileHEVCSccMain10,
    va_backend_sys::VAProfile_VAProfileHEVCSccMain444,
    va_backend_sys::VAProfile_VAProfileAV1Profile0,
    va_backend_sys::VAProfile_VAProfileAV1Profile1,
    va_backend_sys::VAProfile_VAProfileHEVCSccMain444_10,
    va_backend_sys::VAProfile_VAProfileProtected,
    va_backend_sys::VAProfile_VAProfileH264High10,
    va_backend_sys::VAProfile_VAProfileVVCMain10,
    va_backend_sys::VAProfile_VAProfileVVCMultilayerMain10,
];

enum PartialVideoProfileInfo {
    /// VkVideoDecodeH264ProfileInfoKHR
    /// with videCodecOperation = VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR
    H264Decode {
        std_profile_idc: native::StdVideoH264ProfileIdc,
    },
    H265Decode {
        std_profile_idc: native::StdVideoH265ProfileIdc,
    },
    Av1Decode {
        std_profile: native::StdVideoAV1Profile,
    },
}

fn vk_video_profile_info_for_va_profile(va_profile: VAProfile) -> Option<PartialVideoProfileInfo> {
    // Roughly according to <videocodecs> section of the vk.xml registry. See also
    // https://github.com/KhronosGroup/Vulkan-Tools/blob/vulkan-sdk-1.4.321/scripts/vulkaninfo_generator.py#L590
    match va_profile {
        va_backend_sys::VAProfile_VAProfileH264Baseline
        | va_backend_sys::VAProfile_VAProfileH264ConstrainedBaseline => {
            Some(PartialVideoProfileInfo::H264Decode {
                std_profile_idc: native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_BASELINE,
            })
        }
        va_backend_sys::VAProfile_VAProfileH264Main => Some(PartialVideoProfileInfo::H264Decode {
            std_profile_idc: native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN,
        }),
        va_backend_sys::VAProfile_VAProfileH264High => Some(PartialVideoProfileInfo::H264Decode {
            std_profile_idc: native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
        }),
        va_backend_sys::VAProfile_VAProfileHEVCMain => Some(PartialVideoProfileInfo::H265Decode {
            std_profile_idc: native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
        }),
        va_backend_sys::VAProfile_VAProfileHEVCMain10 => {
            Some(PartialVideoProfileInfo::H265Decode {
                std_profile_idc: native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10,
            })
        }
        va_backend_sys::VAProfile_VAProfileAV1Profile0 => {
            Some(PartialVideoProfileInfo::Av1Decode {
                std_profile: native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
            })
        }
        va_backend_sys::VAProfile_VAProfileAV1Profile1 => {
            Some(PartialVideoProfileInfo::Av1Decode {
                std_profile: native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_HIGH,
            })
        }
        _ => None,
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(i32)]
#[allow(dead_code)]
enum VaError {
    OperationFailed = va_backend_sys::VA_STATUS_ERROR_OPERATION_FAILED as VAStatus,
    AllocationFailed = va_backend_sys::VA_STATUS_ERROR_ALLOCATION_FAILED as VAStatus,
    InvalidDisplay = va_backend_sys::VA_STATUS_ERROR_INVALID_DISPLAY as VAStatus,
    InvalidConfig = va_backend_sys::VA_STATUS_ERROR_INVALID_CONFIG as VAStatus,
    InvalidContext = va_backend_sys::VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus,
    InvalidSurface = va_backend_sys::VA_STATUS_ERROR_INVALID_SURFACE as VAStatus,
    InvalidBuffer = va_backend_sys::VA_STATUS_ERROR_INVALID_BUFFER as VAStatus,
    InvalidImage = va_backend_sys::VA_STATUS_ERROR_INVALID_IMAGE as VAStatus,
    InvalidSubpicture = va_backend_sys::VA_STATUS_ERROR_INVALID_SUBPICTURE as VAStatus,
    AttrNotSupported = va_backend_sys::VA_STATUS_ERROR_ATTR_NOT_SUPPORTED as VAStatus,
    MaxNumExceeded = va_backend_sys::VA_STATUS_ERROR_MAX_NUM_EXCEEDED as VAStatus,
    UnsupportedProfile = va_backend_sys::VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus,
    UnsupportedEntrypoint = va_backend_sys::VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT as VAStatus,
    UnsupportedRtformat = va_backend_sys::VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT as VAStatus,
    UnsupportedBuffertype = va_backend_sys::VA_STATUS_ERROR_UNSUPPORTED_BUFFERTYPE as VAStatus,
    SurfaceBusy = va_backend_sys::VA_STATUS_ERROR_SURFACE_BUSY as VAStatus,
    FlagNotSupported = va_backend_sys::VA_STATUS_ERROR_FLAG_NOT_SUPPORTED as VAStatus,
    InvalidParameter = va_backend_sys::VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus,
    ResolutionNotSupported = va_backend_sys::VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED as VAStatus,
    Unimplemented = va_backend_sys::VA_STATUS_ERROR_UNIMPLEMENTED as VAStatus,
    SurfaceInDisplaying = va_backend_sys::VA_STATUS_ERROR_SURFACE_IN_DISPLAYING as VAStatus,
    InvalidImageFormat = va_backend_sys::VA_STATUS_ERROR_INVALID_IMAGE_FORMAT as VAStatus,
    DecodingError = va_backend_sys::VA_STATUS_ERROR_DECODING_ERROR as VAStatus,
    EncodingError = va_backend_sys::VA_STATUS_ERROR_ENCODING_ERROR as VAStatus,
}

impl From<VaError> for VAStatus {
    fn from(err: VaError) -> Self {
        err as VAStatus
    }
}

impl fmt::Display for VaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for VaError {}

/// A device identifier consisting of major and minor numbers.
/// While `major`/`minor` return `u32`, we use `i64` to match the types used by vulkan's
/// `VkPhysicalDeviceDrmPropertiesEXT`, since u32 can trivially be converted to i64 but not vice
/// versa.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct DeviceId(i64, i64);

unsafe fn extract_drm_device_id(driver_context: &mut VADriverContext) -> Result<DeviceId, VaError> {
    // > This structure is allocated from libva with calloc().
    // > All structures shall be derived from struct drm_state.
    let drm_state: *mut drm_state = driver_context.drm_state.cast();

    if drm_state.is_null() || !driver_context.drm_state.is_aligned() {
        error!("driver_context.drm_state is null or unaligned - this is currently not supported");
        return Err(VaError::InvalidParameter);
    }

    let drm_state = unsafe {
        drm_state
            .as_mut()
            .expect("driver_context.drm_state is null after is_null() was checked")
    };

    let drm_fd = RawFd::from(drm_state.fd);

    if drm_fd < 0 {
        error!("Invalid DRM file descriptor: {}", drm_fd);
        return Err(VaError::InvalidParameter);
    }

    info!(
        "DRM state: FD = {drm_fd:?}, Auth type = {:?}",
        drm_state.auth_type
    );

    // Temporarily take ownership of the DRM fd to extract its metadata.
    let drm_file = unsafe { File::from_raw_fd(drm_fd) };
    let metadata = drm_file.metadata();
    let _drm_fd = drm_file.into_raw_fd(); // Prevent the file from being closed when drm_file goes out of scope

    // Extract st_rdev from metadata to identify the device
    // See the implementation of libdrm's drmGetDevice2 for more details

    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(err) => {
            error!("Failed to get metadata for DRM fd {}: {:?}", drm_fd, err);
            return Err(VaError::OperationFailed);
        }
    };

    if !metadata.file_type().is_char_device() {
        error!("DRM fd {} is not a character device", drm_fd);
        return Err(VaError::InvalidParameter);
    }

    let rdev = metadata.st_rdev();

    let major = libc::major(rdev);
    let minor = libc::minor(rdev);

    info!("DRM file has st_rdev {rdev:#x}, which is: major = {major}, minor = {minor}");

    Ok(DeviceId(major.into(), minor.into()))
}

struct DriverData {
    magic: u32,
    vulkan: VulkanData,
}

impl DriverData {
    const MAGIC: u32 = 0x5641564b; // "VAVK"

    unsafe fn from_ptr<'a>(ptr: *mut c_void) -> Result<&'a mut Self, VaError> {
        let ptr: *mut Self = ptr.cast();
        if ptr.is_null() || !ptr.is_aligned() {
            error!("DriverData pointer is null or unaligned");
            return Err(VaError::InvalidParameter);
        }

        let magic = unsafe { (*ptr).magic };
        if magic != Self::MAGIC {
            error!(
                "DriverData magic number mismatch: expected {:#x}, got {:#x}",
                Self::MAGIC,
                magic
            );
            return Err(VaError::InvalidParameter);
        }

        let driver_data = unsafe {
            (ptr as *mut Self)
                .as_mut()
                .expect("DriverData pointer is null after is_null() was checked")
        };

        Ok(driver_data)
    }
}

unsafe fn driver_context_as_ref<'a>(
    driver_context: VADriverContextP,
) -> Result<&'a mut VADriverContext, VaError> {
    if driver_context.is_null() || !driver_context.is_aligned() {
        error!("driver_context is null or not aligned");
        return Err(VaError::InvalidParameter);
    }

    // SAFETY: We checked for null and alignment above, we'll assume that the remaining constraints
    // are satisfied (e.g. that the pointer points to a valid VADriverContext structure).
    let driver_context = unsafe {
        driver_context
            .as_mut()
            .expect("driver_context is null after is_null() was checked")
    };

    Ok(driver_context)
}

unsafe fn va_driver_init(driver_context: VADriverContextP) -> Result<(), VaError> {
    // We expect a valid non-null pointer to an already allocated VADriverContext structure.
    let driver_context = unsafe { driver_context_as_ref(driver_context)? };

    // > This structure is allocated from libva with calloc().
    if driver_context.vtable.is_null() || !driver_context.vtable.is_aligned() {
        error!("driver_context.vtable is null or not aligned");
        return Err(VaError::InvalidParameter);
    }

    let vtable = unsafe {
        driver_context
            .vtable
            .as_mut()
            .expect("driver_context.vtable is null after is_null() was checked")
    };

    // Fill in required attributes.

    println!("{driver_context:#?}");

    // TODO: actual max values
    driver_context.max_profiles = PROFILES.len() as c_int;
    driver_context.max_entrypoints = MAX_ENTRYPOINTS as c_int; // VAEntrypointVLD, VAEntrypointEncSlice
    driver_context.max_attributes = 1;
    driver_context.max_image_formats = 1;
    driver_context.max_subpic_formats = 1;

    driver_context.str_vendor = VENDOR.as_ptr();

    fill_vtable(vtable);

    // Initialize Vulkan and select a physical device matching the DRM device.
    let drm_device_id = unsafe { extract_drm_device_id(driver_context)? };

    let vulkan_data = init_vulkan(drm_device_id).map_err(|err| {
        error!("Failed to initialize Vulkan: {:?}", err);
        VaError::OperationFailed
    })?;

    // Attach our driver data to the context so we can access it in the other functions.
    let driver_data = Box::new(DriverData {
        magic: DriverData::MAGIC,
        vulkan: vulkan_data,
    });
    driver_context.pDriverData = Box::into_raw(driver_data).cast();

    Ok(())
}

/// # Safety
/// This function's safety depends on the caller providing a valid pointer to a
/// `VADriverContext` structure. The function checks for null and alignment, but
/// doesn't (yet) validate the contents of the structure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vaDriverInit_1_22(driver_context: VADriverContextP) -> VAStatus {
    // Initialize the logger. Should only return an error if it's already been initialized.
    let _ = SimpleLogger::new().init();
    log::set_max_level(log::LevelFilter::Trace);

    debug!("__vaDriverInit_1_22 called");

    let result = unsafe { va_driver_init(driver_context) };
    match result {
        Ok(()) => VA_STATUS_SUCCESS as VAStatus,
        Err(err) => {
            // Don't log here, the invoker usually does that
            err.into()
        }
    }
}

/// Compile-time check to ensure the vaDriverInit function conforms to the expected type.
const _DRIVER_INIT: VADriverInit = Some(__vaDriverInit_1_22);
