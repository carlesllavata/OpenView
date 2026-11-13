// SPDX-License-Identifier: GPL-3.0-or-later
//! The messages the program itself writes (errors, stages, dialogs), in English and Spanish.
//! The interface has its own texts in `ui/en.json` and `ui/es.json`.
//!
//! The Spanish texts are those of the original OpenView by Daniel Castellanos (MIT, see NOTICE).

use crate::links::LinkError;
use crate::{hdri, pano, service};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Es,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Es => "es",
        }
    }

    /// "en" or "es"; anything else follows the language of Windows.
    pub fn from_setting(setting: &str) -> Lang {
        match setting {
            "en" => Lang::En,
            "es" => Lang::Es,
            _ => Lang::system(),
        }
    }

    /// Spanish when Windows is in Spanish, English otherwise.
    #[cfg(windows)]
    pub fn system() -> Lang {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        // SAFETY: a Win32 call without arguments that returns a language identifier.
        let id = unsafe { GetUserDefaultUILanguage() };
        // The low ten bits are the primary language; 0x0A is Spanish.
        if id & 0x3ff == 0x0a {
            Lang::Es
        } else {
            Lang::En
        }
    }

    #[cfg(not(windows))]
    pub fn system() -> Lang {
        Lang::En
    }

    fn pick(self, en: &'static str, es: &'static str) -> &'static str {
        match self {
            Lang::En => en,
            Lang::Es => es,
        }
    }
}

/// Fixed messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    AlreadyListed,
    Cancelled,
    SwitchFailed,
    CropPreparing,
    CropDownloading,
    CropExtracting,
    Saving,
    Saved,
    CropGone,
    CropNotReady,
    CropNotForPhotos,
    HdriNoFile,
    HdriTooSmall,
    HdriCancelled,
    CloseTitle,
    PickFolderTitle,
    PickImageTitle,
    PanoramaFiles,
    NoWebView,
}

impl Lang {
    pub fn msg(self, msg: Msg) -> &'static str {
        match msg {
            Msg::AlreadyListed => self.pick("This panorama is already in the list.", "Este panorama ya está en la lista."),
            Msg::Cancelled => self.pick("Download cancelled.", "Descarga cancelada."),
            Msg::SwitchFailed => self.pick("Could not switch date: ", "No se pudo cambiar de fecha: "),
            Msg::CropPreparing => self.pick("Preparing…", "Preparando…"),
            Msg::CropDownloading => self.pick("Downloading the whole panorama…", "Descargando el panorama completo…"),
            Msg::CropExtracting => self.pick("Extracting the crop…", "Extrayendo el recorte…"),
            Msg::Saving => self.pick("Saving…", "Guardando…"),
            Msg::Saved => self.pick("Saved", "Guardado"),
            Msg::CropGone => self.pick("The panorama is no longer in the list.", "El panorama ya no está en la lista."),
            Msg::CropNotReady => self.pick("This panorama is not ready yet.", "Este panorama aún no está listo."),
            Msg::CropNotForPhotos => {
                self.pick("Crops are for 360 panoramas, not for photos.", "Los recortes son para panoramas 360, no para fotos.")
            }
            Msg::HdriNoFile => self.pick("The file was not found.", "No encuentro el archivo."),
            Msg::HdriTooSmall => self.pick("The image is too small.", "La imagen es demasiado pequeña."),
            Msg::HdriCancelled => self.pick("Conversion cancelled.", "Conversión cancelada."),
            Msg::CloseTitle => self.pick("Downloads in progress", "Descargas en curso"),
            Msg::PickFolderTitle => self.pick("Folder to save the panoramas in", "Carpeta donde guardar los panoramas"),
            Msg::PickImageTitle => self.pick("Panorama to convert to HDRI", "Panorama para convertir a HDRI"),
            Msg::PanoramaFiles => self.pick("Panoramas", "Panoramas"),
            Msg::NoWebView => self.pick(
                "OpenView could not open its own window, so it opened in your browser instead.",
                "OpenView no ha podido abrir su propia ventana, así que se ha abierto en el navegador.",
            ),
        }
    }

    /// The question asked when the window is closed with `count` jobs running.
    pub fn close_question(self, count: usize) -> String {
        match (self, count > 1) {
            (Lang::En, false) => "1 download has not finished. If you close now it will be cancelled. Close anyway?".into(),
            (Lang::En, true) => {
                format!("{count} downloads have not finished. If you close now they will be cancelled. Close anyway?")
            }
            (Lang::Es, false) => {
                "Hay 1 descarga sin terminar. Si cierras ahora se cancelará. ¿Cerrar de todas formas?".into()
            }
            (Lang::Es, true) => {
                format!("Hay {count} descargas sin terminar. Si cierras ahora se cancelarán. ¿Cerrar de todas formas?")
            }
        }
    }

    pub fn not_spherical(self, w: u32, h: u32) -> String {
        match self {
            Lang::En => format!("The image is not a 360 panorama (2:1): it is {w} x {h}."),
            Lang::Es => format!("La imagen no es un panorama 360 (2:1): mide {w} x {h}."),
        }
    }

    fn link(self, error: &LinkError) -> String {
        match error {
            LinkError::Empty => self.pick("The link is empty.", "Enlace vacío.").into(),
            LinkError::NotRecognised => self
                .pick(
                    "There is no panorama in this link. Copy the address from the browser's address bar while you are inside the street view.",
                    "No encuentro un panorama en el enlace. Copia la dirección de la barra del navegador estando dentro de la vista de calle.",
                )
                .into(),
        }
    }

    fn io(self, error: &std::io::Error) -> String {
        use std::io::ErrorKind;
        let invalid_folder = matches!(error.raw_os_error(), Some(3 | 123 | 267));
        match error.kind() {
            ErrorKind::PermissionDenied => self
                .pick(
                    "Cannot write to the destination folder (permission denied). Choose another folder, or close the file if it is open in another program.",
                    "No se puede escribir en la carpeta de destino (permiso denegado). Elige otra carpeta o cierra el archivo si lo tienes abierto en otro programa.",
                )
                .into(),
            ErrorKind::StorageFull => {
                self.pick("There is no space left on the destination disk.", "No queda espacio en el disco de destino.").into()
            }
            ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::InvalidFilename => self
                .pick(
                    "The destination folder does not exist or is not valid. Choose another folder.",
                    "La carpeta de destino no existe o no es válida. Elige otra carpeta.",
                )
                .into(),
            _ if invalid_folder => self
                .pick(
                    "The destination folder does not exist or is not valid. Choose another folder.",
                    "La carpeta de destino no existe o no es válida. Elige otra carpeta.",
                )
                .into(),
            _ => self.unexpected(&error.to_string()),
        }
    }

    fn unexpected(self, what: &str) -> String {
        match self {
            Lang::En => format!("Unexpected error: {what}"),
            Lang::Es => format!("Error inesperado: {what}"),
        }
    }

    pub fn service(self, error: &service::Error) -> String {
        use service::{Error, Net};
        match error {
            Error::NotFound => self
                .pick(
                    "The map service does not have this panorama (it may have been removed) or there is no street view at that place. Check that the link is complete.",
                    "El servicio de mapas no devuelve este panorama (puede haber sido retirado) o no hay vista de calle en esa ubicación. Revisa que el enlace esté completo.",
                )
                .into(),
            Error::Network(Net::Offline) => self
                .pick(
                    "There is no Internet connection or the map service does not answer. Check the connection and try again.",
                    "No hay conexión a Internet o el servicio de mapas no responde. Revisa la conexión e inténtalo de nuevo.",
                )
                .into(),
            Error::Network(Net::Timeout) => self
                .pick(
                    "The map service took too long to answer. Try again in a moment.",
                    "El servicio de mapas ha tardado demasiado en responder. Inténtalo de nuevo en un momento.",
                )
                .into(),
            Error::Network(Net::Status(code)) => match self {
                Lang::En => format!("The map service refused the request (error {code}). Try again later."),
                Lang::Es => format!("El servicio de mapas ha rechazado la petición (error {code}). Inténtalo más tarde."),
            },
            Error::Network(Net::Other(what)) | Error::Unexpected(what) => self.unexpected(what),
            Error::Link(e) => self.link(e),
        }
    }

    /// Any failure of a download as a sentence a person can act on.
    pub fn pano(self, error: &pano::Error) -> String {
        match error {
            pano::Error::Cancelled => self.msg(Msg::Cancelled).into(),
            pano::Error::Service(e) => self.service(e),
            pano::Error::Image(what) => self.unexpected(what),
            pano::Error::Io(e) => self.io(e),
        }
    }

    pub fn hdri(self, error: &hdri::Error) -> String {
        match error {
            hdri::Error::Image(what) => self.unexpected(what),
            hdri::Error::Io(e) => self.io(e),
            hdri::Error::NotSpherical(w, h) => self.not_spherical(*w, *h),
            hdri::Error::TooSmall => self.msg(Msg::HdriTooSmall).into(),
            hdri::Error::Cancelled => self.msg(Msg::HdriCancelled).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_come_from_the_setting() {
        assert_eq!(Lang::from_setting("es"), Lang::Es);
        assert_eq!(Lang::from_setting("en"), Lang::En);
        assert_eq!(Lang::from_setting(""), Lang::system());
        assert_eq!((Lang::En.code(), Lang::Es.code()), ("en", "es"));
    }

    #[test]
    fn folder_problems_are_explained() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(Lang::En.pano(&pano::Error::Io(denied)).contains("permission denied"));
        let missing = std::io::Error::from_raw_os_error(3);
        assert!(Lang::Es.pano(&pano::Error::Io(missing)).contains("no existe"));
        let full = std::io::Error::from(std::io::ErrorKind::StorageFull);
        assert!(Lang::En.pano(&pano::Error::Io(full)).contains("no space"));
    }

    #[test]
    fn network_problems_are_explained() {
        let offline = service::Error::Network(service::Net::Offline);
        assert!(Lang::Es.service(&offline).contains("No hay conexión"));
        let refused = service::Error::Network(service::Net::Status(503));
        assert!(Lang::En.service(&refused).contains("error 503"));
    }

    #[test]
    fn the_close_question_counts() {
        assert!(Lang::En.close_question(1).starts_with("1 download has"));
        assert!(Lang::Es.close_question(3).contains("3 descargas"));
    }
}
