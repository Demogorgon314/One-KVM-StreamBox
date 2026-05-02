//! HID Function implementation for USB Gadget

use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use super::configfs::{
    create_dir, create_symlink, remove_dir, remove_file, write_bytes, write_file,
};
use super::function::{FunctionMeta, GadgetFunction};
use super::report_desc::{
    COMPOSITE_KEYBOARD_POINTER, CONSUMER_CONTROL, KEYBOARD, KEYBOARD_WITH_LED, MOUSE_ABSOLUTE,
    MOUSE_RELATIVE,
};
use crate::error::Result;

/// HID function type
#[derive(Debug, Clone)]
pub enum HidFunctionType {
    /// Keyboard
    Keyboard,
    /// Relative mouse (traditional mouse movement)
    /// Uses 1 endpoint: IN
    MouseRelative,
    /// Absolute mouse (touchscreen-like positioning)
    /// Uses 1 endpoint: IN
    MouseAbsolute,
    /// Consumer control (multimedia keys)
    /// Uses 1 endpoint: IN
    ConsumerControl,
    /// Keyboard, relative mouse, absolute mouse, and consumer control over one HID interface.
    CompositeKeyboardPointer,
}

impl HidFunctionType {
    /// Get the endpoint cost for this function type.
    pub fn endpoints(&self, keyboard_leds: bool) -> u8 {
        if self.uses_out_endpoint(keyboard_leds) {
            2
        } else {
            1
        }
    }

    /// Whether this function needs an OUT endpoint.
    pub fn uses_out_endpoint(&self, keyboard_leds: bool) -> bool {
        matches!(self, HidFunctionType::Keyboard) && keyboard_leds
    }

    /// Get HID protocol
    pub fn protocol(&self) -> u8 {
        match self {
            HidFunctionType::Keyboard => 1,                 // Keyboard
            HidFunctionType::MouseRelative => 2,            // Mouse
            HidFunctionType::MouseAbsolute => 2,            // Mouse
            HidFunctionType::ConsumerControl => 0,          // None
            HidFunctionType::CompositeKeyboardPointer => 0, // None
        }
    }

    /// Get HID subclass
    pub fn subclass(&self) -> u8 {
        match self {
            HidFunctionType::Keyboard => 1,                 // Boot interface
            HidFunctionType::MouseRelative => 1,            // Boot interface
            HidFunctionType::MouseAbsolute => 0,            // No boot interface
            HidFunctionType::ConsumerControl => 0,          // No boot interface
            HidFunctionType::CompositeKeyboardPointer => 0, // No boot interface
        }
    }

    /// Get report length in bytes
    pub fn report_length(&self, _keyboard_leds: bool) -> u8 {
        match self {
            HidFunctionType::Keyboard => 8,
            HidFunctionType::MouseRelative => 4,
            HidFunctionType::MouseAbsolute => 6,
            HidFunctionType::ConsumerControl => 2,
            HidFunctionType::CompositeKeyboardPointer => 9,
        }
    }

    /// Get report descriptor
    pub fn report_desc(&self, keyboard_leds: bool) -> &'static [u8] {
        match self {
            HidFunctionType::Keyboard => {
                if keyboard_leds {
                    KEYBOARD_WITH_LED
                } else {
                    KEYBOARD
                }
            }
            HidFunctionType::MouseRelative => MOUSE_RELATIVE,
            HidFunctionType::MouseAbsolute => MOUSE_ABSOLUTE,
            HidFunctionType::ConsumerControl => CONSUMER_CONTROL,
            HidFunctionType::CompositeKeyboardPointer => COMPOSITE_KEYBOARD_POINTER,
        }
    }

    /// Get description
    pub fn description(&self) -> &'static str {
        match self {
            HidFunctionType::Keyboard => "Keyboard",
            HidFunctionType::MouseRelative => "Relative Mouse",
            HidFunctionType::MouseAbsolute => "Absolute Mouse",
            HidFunctionType::ConsumerControl => "Consumer Control",
            HidFunctionType::CompositeKeyboardPointer => "Composite Keyboard/Pointer",
        }
    }
}

/// HID Function for USB Gadget
///
/// On Amlogic kernels, ConfigFS HID function directories must use
/// well-known names (`hid.keyboard`, `hid.mouse`) rather than the
/// generic `hid.usb{N}` convention.  This struct stores both the
/// ConfigFS function name and the device-node index separately so
/// that the two can diverge when needed.
pub struct HidFunction {
    /// Minor device index → /dev/hidg{instance}
    instance: u8,
    /// Function type
    func_type: HidFunctionType,
    /// ConfigFS function directory name (e.g. `hid.keyboard`, `hid.mouse`)
    name: String,
    /// Whether keyboard LED/status feedback is enabled.
    keyboard_leds: bool,
}

impl HidFunction {
    /// Create a keyboard function → ConfigFS name `hid.keyboard`
    pub fn keyboard(instance: u8, keyboard_leds: bool) -> Self {
        Self {
            instance,
            func_type: HidFunctionType::Keyboard,
            name: "hid.keyboard".to_string(),
            keyboard_leds,
        }
    }

    /// Create a relative mouse function → ConfigFS name `hid.mouse`
    pub fn mouse_relative(instance: u8) -> Self {
        Self {
            instance,
            func_type: HidFunctionType::MouseRelative,
            name: "hid.mouse".to_string(),
            keyboard_leds: false,
        }
    }

    /// Create an absolute mouse function
    pub fn mouse_absolute(instance: u8) -> Self {
        Self {
            instance,
            func_type: HidFunctionType::MouseAbsolute,
            name: format!("hid.usb{}", instance),
            keyboard_leds: false,
        }
    }

    /// Create a consumer control function
    pub fn consumer_control(instance: u8) -> Self {
        Self {
            instance,
            func_type: HidFunctionType::ConsumerControl,
            name: format!("hid.usb{}", instance),
            keyboard_leds: false,
        }
    }

    /// Create a composite keyboard/pointer function using a single HID interface.
    pub fn composite_keyboard_pointer(instance: u8) -> Self {
        Self {
            instance,
            func_type: HidFunctionType::CompositeKeyboardPointer,
            name: "hid.keyboard".to_string(),
            keyboard_leds: false,
        }
    }

    fn function_path(&self, gadget_path: &Path) -> PathBuf {
        gadget_path.join("functions").join(self.name())
    }

    pub fn device_path(&self) -> PathBuf {
        PathBuf::from(format!("/dev/hidg{}", self.instance))
    }
}

impl GadgetFunction for HidFunction {
    fn name(&self) -> &str {
        &self.name
    }

    fn endpoints_required(&self) -> u8 {
        self.func_type.endpoints(self.keyboard_leds)
    }

    fn meta(&self) -> FunctionMeta {
        FunctionMeta {
            name: self.name().to_string(),
            description: self.func_type.description().to_string(),
            endpoints: self.endpoints_required(),
            enabled: true,
        }
    }

    fn create(&self, gadget_path: &Path) -> Result<()> {
        let func_path = self.function_path(gadget_path);
        create_dir(&func_path)?;

        // Set HID parameters
        write_file(
            &func_path.join("protocol"),
            &self.func_type.protocol().to_string(),
        )?;
        write_file(
            &func_path.join("subclass"),
            &self.func_type.subclass().to_string(),
        )?;
        write_file(
            &func_path.join("report_length"),
            &self.func_type.report_length(self.keyboard_leds).to_string(),
        )?;

        if !self.func_type.uses_out_endpoint(self.keyboard_leds) {
            if let Err(err) = write_file(&func_path.join("no_out_endpoint"), "1") {
                warn!(
                    "Failed to disable OUT endpoint for HID function {}: {}",
                    self.name(),
                    err
                );
            }
        }

        // Write report descriptor
        write_bytes(
            &func_path.join("report_desc"),
            self.func_type.report_desc(self.keyboard_leds),
        )?;

        debug!(
            "Created HID function: {} at {} (endpoints: {})",
            self.name(),
            func_path.display(),
            self.endpoints_required()
        );
        Ok(())
    }

    fn link(&self, config_path: &Path, gadget_path: &Path) -> Result<()> {
        let func_path = self.function_path(gadget_path);
        let link_path = config_path.join(self.name());

        if !link_path.exists() {
            create_symlink(&func_path, &link_path)?;
            debug!("Linked HID function {} to config", self.name());
        }

        Ok(())
    }

    fn unlink(&self, config_path: &Path) -> Result<()> {
        let link_path = config_path.join(self.name());
        remove_file(&link_path)?;
        debug!("Unlinked HID function {}", self.name());
        Ok(())
    }

    fn cleanup(&self, gadget_path: &Path) -> Result<()> {
        let func_path = self.function_path(gadget_path);
        remove_dir(&func_path)?;
        debug!("Cleaned up HID function {}", self.name());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hid_function_types() {
        assert_eq!(HidFunctionType::Keyboard.endpoints(true), 2);
        assert_eq!(HidFunctionType::Keyboard.endpoints(false), 1);
        assert_eq!(HidFunctionType::MouseRelative.endpoints(false), 1);
        assert_eq!(HidFunctionType::MouseAbsolute.endpoints(false), 1);
        assert_eq!(
            HidFunctionType::CompositeKeyboardPointer.endpoints(false),
            1
        );

        assert_eq!(HidFunctionType::Keyboard.report_length(false), 8);
        assert_eq!(HidFunctionType::Keyboard.report_length(true), 8);
        assert_eq!(HidFunctionType::MouseRelative.report_length(false), 4);
        assert_eq!(HidFunctionType::MouseAbsolute.report_length(false), 6);
        assert_eq!(
            HidFunctionType::CompositeKeyboardPointer.report_length(false),
            9
        );
    }

    #[test]
    fn test_hid_function_names() {
        let kb = HidFunction::keyboard(0, false);
        assert_eq!(kb.name(), "hid.keyboard");
        assert_eq!(kb.device_path(), PathBuf::from("/dev/hidg0"));

        let mouse = HidFunction::mouse_relative(1);
        assert_eq!(mouse.name(), "hid.mouse");
        assert_eq!(mouse.device_path(), PathBuf::from("/dev/hidg1"));

        let composite = HidFunction::composite_keyboard_pointer(0);
        assert_eq!(composite.name(), "hid.keyboard");
        assert_eq!(composite.device_path(), PathBuf::from("/dev/hidg0"));
    }
}
