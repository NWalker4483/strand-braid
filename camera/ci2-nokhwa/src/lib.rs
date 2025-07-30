extern crate machine_vision_formats as formats;
use std::sync::{Arc, Mutex};

use ci2::{
    AcquisitionMode, AutoMode, DynamicFrameWithInfo, HostTimingInfo, TriggerMode, TriggerSelector,
};
use nokhwa::{
    pixel_format::RgbFormat,
    utils::{
        ApiBackend, CameraFormat, CameraIndex, CameraInfo, FrameFormat, KnownCameraControl,
        RequestedFormat, RequestedFormatType,
    },
    Camera,
};

pub type Result<M> = std::result::Result<M, Error>;
use strand_dynamic_frame::DynamicFrameOwned;
use tracing::debug;

const BAD_FNO: usize = usize::MAX;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Nokhwa error: {source}")]
    NokhwaError {
        #[from]
        source: nokhwa::NokhwaError,
    },
    #[error("int parse error: {source}")]
    IntParseError {
        #[from]
        source: std::num::ParseIntError,
    },
    #[error("other error: {msg}")]
    OtherError { msg: String },
}

impl From<Error> for ci2::Error {
    fn from(orig: Error) -> ci2::Error {
        ci2::Error::BackendError(orig.into())
    }
}

pub struct WrappedModule {
    // Nokhwa doesn't need a persistent module state like Pylon
}

fn to_name(info: &CameraInfo) -> String {
    format!("{}-{}", info.human_name(), info.index())
}

pub fn new_module() -> ci2::Result<WrappedModule> {
    Ok(WrappedModule {})
}

pub struct NokhwaTerminateGuard {
    already_dropped: bool,
}

impl Drop for NokhwaTerminateGuard {
    fn drop(&mut self) {
        if !self.already_dropped {
            // Nokhwa doesn't require explicit termination
            self.already_dropped = true;
        }
    }
}

pub fn make_singleton_guard(
    _module: &dyn ci2::CameraModule<CameraType = WrappedCamera, Guard = NokhwaTerminateGuard>,
) -> ci2::Result<NokhwaTerminateGuard> {
    Ok(NokhwaTerminateGuard {
        already_dropped: false,
    })
}

impl<'a> ci2::CameraModule for &'a WrappedModule {
    type CameraType = WrappedCamera;
    type Guard = NokhwaTerminateGuard;

    fn name(self: &&'a WrappedModule) -> &'static str {
        "nokhwa"
    }

    fn camera_infos(self: &&'a WrappedModule) -> ci2::Result<Vec<Box<dyn ci2::CameraInfo>>> {
        let nokhwa_infos = nokhwa::query(ApiBackend::Auto).map_err(Error::from)?;

        let infos = nokhwa_infos
            .into_iter()
            .map(|info| {
                let name = to_name(&info);
                let serial = format!("{}", info.index()); // Nokhwa uses index as identifier
                let model = info.human_name().to_string();
                let vendor = "Unknown".to_string(); // Nokhwa doesn't always provide vendor info

                let nci = Box::new(NokhwaCameraInfo {
                    name,
                    serial,
                    model,
                    vendor,
                });
                let ci: Box<dyn ci2::CameraInfo> = nci;
                ci
            })
            .collect();
        Ok(infos)
    }

    fn camera(self: &mut &'a WrappedModule, name: &str) -> ci2::Result<Self::CameraType> {
        WrappedCamera::new(name)
    }

    fn settings_file_extension(&self) -> &str {
        "json"
    }
}

#[derive(Debug)]
struct NokhwaCameraInfo {
    name: String,
    serial: String,
    model: String,
    vendor: String,
}

impl ci2::CameraInfo for NokhwaCameraInfo {
    fn name(&self) -> &str {
        &self.name
    }
    fn serial(&self) -> &str {
        &self.serial
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn vendor(&self) -> &str {
        &self.vendor
    }
}

// Create a Send wrapper for Camera
struct SendableCamera {
    inner: Camera,
}

// SAFETY: This is a workaround for nokhwa's Camera not implementing Send.
// We're asserting that it's safe to send between threads, but this should be used carefully.
// In practice, you should ensure proper synchronization when using this across threads.
unsafe impl Send for SendableCamera {}

impl SendableCamera {
    fn new(camera: Camera) -> Self {
        Self { inner: camera }
    }

    fn get_mut(&mut self) -> &mut Camera {
        &mut self.inner
    }

    fn get(&self) -> &Camera {
        &self.inner
    }
}

pub struct WrappedCamera {
    inner: Arc<Mutex<SendableCamera>>,
    store_fno: Arc<Mutex<usize>>,
    name: String,
    serial: String,
    model: String,
    vendor: String,
    current_format: Arc<Mutex<CameraFormat>>,
}

fn _test_camera_is_send() {
    // Compile-time test to ensure WrappedCamera implements Send trait.
    fn implements<T: Send>() {}
    implements::<WrappedCamera>();
}

impl WrappedCamera {
    fn new(name: &str) -> ci2::Result<Self> {
        let max_u64_as_usize: usize = u64::MAX.try_into().unwrap();
        assert_eq!(max_u64_as_usize, BAD_FNO);

        let devices = nokhwa::query(ApiBackend::Auto).map_err(Error::from)?;

        for device_info in devices.into_iter() {
            let this_name = to_name(&device_info);
            if this_name == name {
                let serial = format!("{}", device_info.index());
                let model = device_info.human_name().to_string();
                let vendor = "Unknown".to_string();
                let store_fno = 0;

                // Create camera with default format
                let index = match device_info.index() {
                    CameraIndex::Index(i) => CameraIndex::Index(*i),
                    CameraIndex::String(s) => CameraIndex::String(s.to_string()),
                };

                let requested = RequestedFormat::new::<RgbFormat>(
                    RequestedFormatType::AbsoluteHighestFrameRate,
                );
                let camera = Camera::new(index, requested).map_err(Error::from)?;

                let current_format = camera.camera_format();

                return Ok(Self {
                    inner: Arc::new(Mutex::new(SendableCamera::new(camera))),
                    name: name.to_string(),
                    store_fno: Arc::new(Mutex::new(store_fno)),
                    serial,
                    model,
                    vendor,
                    current_format: Arc::new(Mutex::new(current_format)),
                });
            }
        }

        Err(Error::OtherError {
            msg: format!("requested camera '{}' was not found", name),
        }
        .into())
    }
}

impl ci2::CameraInfo for WrappedCamera {
    fn name(&self) -> &str {
        &self.name
    }
    fn serial(&self) -> &str {
        &self.serial
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn vendor(&self) -> &str {
        &self.vendor
    }
}

impl ci2::Camera for WrappedCamera {
    // ----- start: weakly typed but easier to implement API -----

    fn command_execute(&self, name: &str, _verify: bool) -> ci2::Result<()> {
        debug!("Attempted to execute:{} ", name);
        match name {
            "AcquisitionStart" => {
                let mut camera = self.inner.lock().unwrap();
                camera.get_mut().open_stream().map_err(Error::from)?;
                Ok(())
            }
            "AcquisitionStop" => {
                let mut camera = self.inner.lock().unwrap();
                camera.get_mut().stop_stream().map_err(Error::from)?;
                Ok(())
            }
            _ => Err(ci2::Error::from(format!("Unknown command: {}", name))),
        }
    }

    fn feature_bool(&self, name: &str) -> ci2::Result<bool> {
        match name {
            _ => {
                // For unknown boolean features, return false as default
                Ok(false)
            }
        }
    }

    fn feature_bool_set(&self, name: &str, value: bool) -> ci2::Result<()> {
        debug!("Attempted to set feature:{} to {}", name, value);
        match name {
            _ => {
                // Ignore unknown boolean settings
                Ok(())
            }
        }
    }

    fn feature_enum(&self, name: &str) -> ci2::Result<String> {
        debug!("Attempted to get feature:{} ", name);
        match name {
            "PixelFormat" => {
                let format = self.current_format.lock().unwrap();
                Ok(convert_from_nokhwa_format(format.format()))
            }
            "TriggerMode" => Ok("Off".to_string()), // Nokhwa doesn't support triggers
            "AcquisitionMode" => Ok("Continuous".to_string()),
            "TriggerSelector" => Ok("FrameStart".to_string()),
            "ExposureAuto" => {
                let camera = self.inner.lock().unwrap();
                // Try to get exposure control, default to Off if not available
                if let Ok(_control) = camera.get().camera_control(KnownCameraControl::Exposure) {
                    // For simplicity, assume manual mode. In a real implementation,
                    // you'd check the control flags
                    Ok("Off".to_string())
                } else {
                    Ok("Off".to_string())
                }
            }
            "GainAuto" => {
                let camera = self.inner.lock().unwrap();
                // Try to get gain control, default to Off if not available
                if let Ok(_control) = camera.get().camera_control(KnownCameraControl::Gain) {
                    // For simplicity, assume manual mode
                    Ok("Off".to_string())
                } else {
                    Ok("Off".to_string())
                }
            }
            _ => Err(ci2::Error::from(format!("Unknown enum feature: {}", name))),
        }
    }

    fn feature_enum_set(&self, name: &str, value: &str) -> ci2::Result<()> {
        debug!("Attempted to set feature:{} to {}", name, value);
        match name {
            "PixelFormat" => {
                let format = convert_to_nokhwa_format(value)?;
                let mut current_format = self.current_format.lock().unwrap();
                *current_format = CameraFormat::new(
                    current_format.resolution(),
                    format,
                    current_format.frame_rate(),
                );

                let mut camera = self.inner.lock().unwrap();
                let requested =
                    RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(*current_format));
                camera
                    .get_mut()
                    .set_camera_requset(requested)
                    .map_err(Error::from)?;
                Ok(())
            }
            "ExposureAuto" | "GainAuto" => {
                // Note: Setting auto modes would require actual control changes
                // For now, just accept the values
                Ok(())
            }
            _ => Ok(()), // Ignore unsupported enum settings
        }
    }

    fn feature_float(&self, name: &str) -> ci2::Result<f64> {
        debug!("Attempted to get feature:{} ", name);
        match name {
            "ExposureTime" => {
                // Default exposure time in microseconds if not available
                Ok(1000.0)
            }
            "Gain" => {
                // Default gain in dB if not available
                Ok(0.0)
            }
            "AcquisitionFrameRate" | "AcquisitionFrameRateAbs" => {
                let format = self.current_format.lock().unwrap();
                Ok(format.frame_rate() as f64)
            }
            _ => Err(ci2::Error::from(format!("Unknown float feature: {}", name))),
        }
    }

    fn feature_float_set(&self, name: &str, value: f64) -> ci2::Result<()> {
        debug!("Attempted to set feature:{} to {}", name, value);
        match name {
            "ExposureTime" | "Gain" => {
                // Note: Setting these values would require actual control manipulation
                // For now, just accept the values
                Ok(())
            }
            "AcquisitionFrameRate" | "AcquisitionFrameRateAbs" => {
                // let mut current_format = self.current_format.lock().unwrap();
                // *current_format = CameraFormat::new(
                //     current_format.resolution(),
                //     current_format.format(),
                //     value as u32,
                // );

                // let mut camera = self.inner.lock().unwrap();
                // let requested =
                //     RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(*current_format));
                // camera
                //     .get_mut()
                //     .set_camera_requset(requested)
                //     .map_err(Error::from)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn feature_int(&self, name: &str) -> ci2::Result<i64> {
        debug!("Attempted to get feature:{} ", name);
        match name {
            "Width" => Ok(self.width()? as i64),
            "Height" => Ok(self.height()? as i64),
            _ => {
                // For unknown integer features, return 0 as default
                Ok(0)
            }
        }
    }

    fn feature_int_set(&self, name: &str, value: i64) -> ci2::Result<()> {
        debug!("Attempted to set feature:{} to {}", name, value);
        // Note: Setting integer values would require actual implementation
        // For now, just accept all values
        Ok(())
    }

    // ----- end: weakly typed but easier to implement API -----

    fn node_map_load(&self, _settings: &str) -> ci2::Result<()> {
        // For nokhwa, we could parse JSON settings and apply them
        // This is a simplified implementation
        tracing::warn!("node_map_load not fully implemented for nokhwa");
        Ok(())
    }

    fn node_map_save(&self) -> ci2::Result<String> {
        // For nokhwa, we could serialize current settings to JSON
        // This is a simplified implementation
        Ok("{}".to_string())
    }

    fn width(&self) -> ci2::Result<u32> {
        let format = self.current_format.lock().unwrap();
        Ok(format.resolution().width())
    }

    fn height(&self) -> ci2::Result<u32> {
        let format = self.current_format.lock().unwrap();
        Ok(format.resolution().height())
    }

    // Settings: PixFmt ----------------------------
    fn pixel_format(&self) -> ci2::Result<formats::PixFmt> {
        let format = self.current_format.lock().unwrap();
        convert_nokhwa_to_machine_vision_format(format.format())
    }

    fn possible_pixel_formats(&self) -> ci2::Result<Vec<formats::PixFmt>> {
        // Nokhwa supports various formats, return common ones
        Ok(vec![
            formats::PixFmt::RGB8,
            formats::PixFmt::Mono8,
            formats::PixFmt::YUV422,
        ])
    }

    fn set_pixel_format(&mut self, pixel_format: formats::PixFmt) -> ci2::Result<()> {
        let nokhwa_format = convert_machine_vision_to_nokhwa_format(pixel_format)?;
        let mut current_format = self.current_format.lock().unwrap();
        *current_format = CameraFormat::new(
            current_format.resolution(),
            nokhwa_format,
            current_format.frame_rate(),
        );

        // Update the camera format using the new API
        let mut camera = self.inner.lock().unwrap();
        let requested =
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(*current_format));
        camera
            .get_mut()
            .set_camera_requset(requested)
            .map_err(Error::from)?;
        Ok(())
    }

    // Settings: Exposure Time ----------------------------
    fn exposure_time(&self) -> ci2::Result<f64> {
        self.feature_float("ExposureTime")
    }

    fn exposure_time_range(&self) -> ci2::Result<(f64, f64)> {
        // Default range in microseconds for webcams
        Ok((1.0, 10000.0))
    }

    fn set_exposure_time(&mut self, value: f64) -> ci2::Result<()> {
        self.feature_float_set("ExposureTime", value)
    }

    // Settings: Exposure Time Auto Mode ----------------------------
    fn exposure_auto(&self) -> ci2::Result<AutoMode> {
        let val = self.feature_enum("ExposureAuto")?;
        str_to_auto_mode(&val)
    }

    fn set_exposure_auto(&mut self, value: AutoMode) -> ci2::Result<()> {
        let sval = mode_to_str(value);
        self.feature_enum_set("ExposureAuto", sval)
    }

    // Settings: Gain ----------------------------
    fn gain(&self) -> ci2::Result<f64> {
        self.feature_float("Gain")
    }

    fn gain_range(&self) -> ci2::Result<(f64, f64)> {
        // Default range for webcam gain in dB
        Ok((0.0, 100.0))
    }

    fn set_gain(&mut self, gain_db: f64) -> ci2::Result<()> {
        self.feature_float_set("Gain", gain_db)
    }

    // Settings: Gain Auto Mode ----------------------------
    fn gain_auto(&self) -> ci2::Result<AutoMode> {
        let val = self.feature_enum("GainAuto")?;
        str_to_auto_mode(&val)
    }

    fn set_gain_auto(&mut self, value: AutoMode) -> ci2::Result<()> {
        let sval = mode_to_str(value);
        self.feature_enum_set("GainAuto", sval)
    }

    // Settings: TriggerMode ----------------------------
    fn trigger_mode(&self) -> ci2::Result<TriggerMode> {
        // Nokhwa doesn't support hardware triggers, always return Off
        Ok(ci2::TriggerMode::Off)
    }

    fn set_trigger_mode(&mut self, _value: TriggerMode) -> ci2::Result<()> {
        // Nokhwa doesn't support hardware triggers, ignore
        Ok(())
    }

    // Settings: AcquisitionFrameRateEnable ----------------------------
    fn acquisition_frame_rate_enable(&self) -> ci2::Result<bool> {
        Ok(true) // Always enabled in nokhwa
    }

    fn set_acquisition_frame_rate_enable(&mut self, _value: bool) -> ci2::Result<()> {
        Ok(()) // Always enabled in nokhwa
    }

    // Settings: AcquisitionFrameRate ----------------------------
    fn acquisition_frame_rate(&self) -> ci2::Result<f64> {
        self.feature_float("AcquisitionFrameRate")
    }

    fn acquisition_frame_rate_range(&self) -> ci2::Result<(f64, f64)> {
        Ok((1.0, 120.0)) // Common range for webcams
    }

    fn set_acquisition_frame_rate(&mut self, value: f64) -> ci2::Result<()> {
        self.feature_float_set("AcquisitionFrameRate", value)
    }

    // Settings: TriggerSelector ----------------------------
    fn trigger_selector(&self) -> ci2::Result<TriggerSelector> {
        Ok(ci2::TriggerSelector::FrameStart) // Default
    }

    fn set_trigger_selector(&mut self, _value: TriggerSelector) -> ci2::Result<()> {
        Ok(()) // Nokhwa doesn't support trigger selectors
    }

    // Settings: AcquisitionMode ----------------------------
    fn acquisition_mode(&self) -> ci2::Result<AcquisitionMode> {
        Ok(ci2::AcquisitionMode::Continuous) // Nokhwa is always continuous
    }

    fn set_acquisition_mode(&mut self, _value: ci2::AcquisitionMode) -> ci2::Result<()> {
        Ok(()) // Nokhwa is always continuous
    }

    // Acquisition ----------------------------
    fn acquisition_start(&mut self) -> ci2::Result<()> {
        let mut camera = self.inner.lock().unwrap();
        camera.get_mut().open_stream().map_err(Error::from)?;
        Ok(())
    }

    fn acquisition_stop(&mut self) -> ci2::Result<()> {
        let mut camera = self.inner.lock().unwrap();
        camera.get_mut().stop_stream().map_err(Error::from)?;
        Ok(())
    }

    /// synchronous (blocking) frame acquisition
    fn next_frame(&mut self) -> ci2::Result<DynamicFrameWithInfo> {
        let mut camera = self.inner.lock().unwrap();
        let frame = camera.get_mut().frame().map_err(Error::from)?;
        let now = chrono::Utc::now();

        let mut fno_guard = self.store_fno.lock().unwrap();
        let fno: usize = *fno_guard;
        *fno_guard += 1;
        drop(fno_guard);

        let width = frame.resolution().width();
        let height = frame.resolution().height();
        let _pixel_format = convert_nokhwa_to_machine_vision_format(frame.source_frame_format())?;

        // Convert frame to RGB8 for consistency
        let rgb_frame = frame.decode_image::<RgbFormat>().map_err(Error::from)?;
        let image_data = rgb_frame.as_raw().to_vec();
        let stride = width * 3; // RGB8 has 3 bytes per pixel

        let host_timing = HostTimingInfo { fno, datetime: now };
        let image = Arc::new(
            DynamicFrameOwned::from_buf(
                width,
                height,
                stride.try_into().unwrap(),
                image_data,
                formats::PixFmt::RGB8,
            )
            .unwrap(),
        );

        Ok(DynamicFrameWithInfo {
            image,
            host_timing,
            backend_data: None,
        })
    }

    fn start_default_external_triggering(&mut self) -> ci2::Result<()> {
        // This is the generic default implementation which may be overriden by
        // implementors.

        // The trigger selector must be set before the trigger mode.
        self.set_trigger_selector(TriggerSelector::FrameStart)?;
        self.set_trigger_mode(TriggerMode::On)
    }

    fn set_software_frame_rate_limit(&mut self, fps_limit: f64) -> ci2::Result<()> {
        // This is the generic default implementation which may be overriden by
        // implementors.
        self.set_acquisition_frame_rate_enable(true)?;
        self.set_acquisition_frame_rate(fps_limit)
    }
}

// Conversion functions between nokhwa and machine_vision_formats
fn convert_nokhwa_to_machine_vision_format(format: FrameFormat) -> ci2::Result<formats::PixFmt> {
    use formats::PixFmt::*;
    match format {
        FrameFormat::MJPEG => Ok(RGB8), // MJPEG will be decoded to RGB
        FrameFormat::YUYV => Ok(YUV422),
        FrameFormat::GRAY => Ok(Mono8),
        _ => Ok(RGB8), // Default to RGB8 for unknown formats
    }
}

fn convert_machine_vision_to_nokhwa_format(format: formats::PixFmt) -> ci2::Result<FrameFormat> {
    use formats::PixFmt::*;
    match format {
        RGB8 => Ok(FrameFormat::MJPEG),
        YUV422 => Ok(FrameFormat::YUYV),
        Mono8 => Ok(FrameFormat::GRAY),
        _ => Err(ci2::Error::from(format!(
            "Unsupported pixel format: {}",
            format
        ))),
    }
}

fn convert_from_nokhwa_format(format: FrameFormat) -> String {
    match format {
        FrameFormat::MJPEG => "MJPEG".to_string(),
        FrameFormat::YUYV => "YUYV".to_string(),
        FrameFormat::GRAY => "GRAY".to_string(),
        _ => format!("{:?}", format),
    }
}

fn convert_to_nokhwa_format(format_str: &str) -> ci2::Result<FrameFormat> {
    match format_str {
        "MJPEG" => Ok(FrameFormat::MJPEG),
        "YUYV" => Ok(FrameFormat::YUYV),
        "GRAY" => Ok(FrameFormat::GRAY),
        _ => Err(ci2::Error::from(format!(
            "Unknown format string: {}",
            format_str
        ))),
    }
}

fn str_to_auto_mode(val: &str) -> ci2::Result<ci2::AutoMode> {
    match val {
        "Off" => Ok(ci2::AutoMode::Off),
        "Once" => Ok(ci2::AutoMode::Once),
        "Continuous" => Ok(ci2::AutoMode::Continuous),
        s => Err(ci2::Error::from(format!(
            "unexpected AutoMode enum string: {}",
            s
        ))),
    }
}

fn mode_to_str(value: AutoMode) -> &'static str {
    match value {
        ci2::AutoMode::Off => "Off",
        ci2::AutoMode::Once => "Once",
        ci2::AutoMode::Continuous => "Continuous",
    }
}
