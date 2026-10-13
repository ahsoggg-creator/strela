use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ввод-вывод: {0}")]
    Io(#[from] io::Error),
    #[error("{path}: {source}")]
    At {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("NTFS: {0}")]
    Ntfs(String),
    #[error("не поддерживается: {0}")]
    Unsupported(String),
    #[error("запрещено защитой: {0}")]
    Protected(String),
    #[error("целостность нарушена: {0}")]
    Verify(String),
    #[error("запрос: {0}")]
    Query(String),
    #[error("отменено")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

pub trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::At {
            path: path.into(),
            source,
        })
    }
}
