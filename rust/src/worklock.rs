//! Cerrojo sobre la carpeta de trabajo.
//!
//! Dos ejecuciones a la vez sobre el mismo `work/` se pisan: yt-dlp escribe el
//! mismo `.part`, no puede renombrarlo y falla despues de minutos descargando.
//! Con el cerrojo la segunda ejecucion falla en el primer segundo con un
//! mensaje claro.
//!
//! Se apoya en el bloqueo de ficheros del sistema operativo, no en "existe el
//! fichero": si el proceso muere de golpe, el sistema suelta el cerrojo solo.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error(
    "La carpeta de trabajo '{}' ya la esta usando otra ejecucion (otra ventana, la \
     interfaz, o un proceso que se quedo colgado).\n  - Espera a que termine, o\n  \
     - lanza esta con otra carpeta: paths.work_dir en el config.yaml.",
    .0.display()
)]
pub struct WorkDirBusy(PathBuf);

#[derive(Debug)]
pub struct WorkLock {
    work_dir: PathBuf,
    file: Option<File>,
}

impl WorkLock {
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self { work_dir: work_dir.into(), file: None }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn acquire(&mut self) -> Result<(), WorkDirBusy> {
        if self.file.is_some() {
            return Ok(());
        }
        let busy = || WorkDirBusy(self.work_dir.clone());
        fs::create_dir_all(&self.work_dir).map_err(|_| busy())?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.work_dir.join(".lock"))
            .map_err(|_| busy())?;
        match file.try_lock() {
            Ok(()) => {
                self.file = Some(file);
                Ok(())
            }
            Err(TryLockError::WouldBlock) | Err(TryLockError::Error(_)) => Err(busy()),
        }
    }

    pub fn release(&mut self) {
        // Cerrar el descriptor suelta el bloqueo. El .lock no se borra: en
        // Windows otro proceso puede tenerlo abierto.
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

impl Drop for WorkLock {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_segunda_ejecucion_no_entra() {
        let dir = tempfile::tempdir().unwrap();
        let mut primera = WorkLock::new(dir.path().join("work"));
        let mut segunda = WorkLock::new(dir.path().join("work"));

        primera.acquire().unwrap();
        let error = segunda.acquire().unwrap_err().to_string();
        assert!(error.contains("paths.work_dir"), "{error}");
    }

    #[test]
    fn al_soltar_otra_ejecucion_puede_entrar() {
        let dir = tempfile::tempdir().unwrap();
        let mut primera = WorkLock::new(dir.path());
        primera.acquire().unwrap();
        primera.release();

        let mut segunda = WorkLock::new(dir.path());
        segunda.acquire().unwrap();
    }

    #[test]
    fn soltar_al_salir_del_ambito() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut lock = WorkLock::new(dir.path());
            lock.acquire().unwrap();
        }
        WorkLock::new(dir.path()).acquire().unwrap();
    }

    #[test]
    fn carpetas_distintas_no_se_molestan() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = WorkLock::new(dir.path().join("a"));
        let mut b = WorkLock::new(dir.path().join("b"));
        a.acquire().unwrap();
        b.acquire().unwrap();
    }

    #[test]
    fn adquirir_dos_veces_es_inofensivo() {
        let dir = tempfile::tempdir().unwrap();
        let mut lock = WorkLock::new(dir.path());
        lock.acquire().unwrap();
        lock.acquire().unwrap();
    }
}
