use std::fmt::{Debug, Formatter};
use std::ops;
use std::path::{Path, PathBuf};

pub const NEW_ROOT_DIR: &str = "new-root";
pub const OLD_ROOT_DIR: &str = "old-root";
pub const ROOT_INIT_PATH: &str = "/tmp";

pub struct _Path {
    inner: PathBuf,
}

impl _Path {
    pub(crate) fn as_path(&self) -> &Path {
        self.inner.as_path()
    }
    pub(crate) fn from_iter<P: AsRef<Path>, I: IntoIterator<Item=P>>(iter: I) -> _Path {
        _Path {
            inner: PathBuf::from_iter(iter)
        }
    }
}

impl From<&str> for _Path {
    fn from(path: &str) -> Self {
        _Path {
            inner: PathBuf::from(path)
        }
    }
}

impl<'x> From<&'x _Path> for &'x Path {
    fn from(path: &'x _Path) -> Self {
        path.as_path()
    }
}

impl<'x> From<&'x _Path> for &'x str {
    fn from(path: &'x _Path) -> Self {
        path.inner.to_str().unwrap()
    }
}

impl ops::Div<&str> for &_Path {
    type Output = _Path;

    fn div(self, rhs: &str) -> Self::Output {
        let mut z = self.inner.to_owned();
        z.push(rhs);
        return _Path {
            inner: z
        };
    }
}

impl ops::Div<&str> for _Path {
    type Output = _Path;

    fn div(mut self, rhs: &str) -> Self::Output {
        self.inner.push(rhs);
        return self;
    }
}

impl ops::Div<&Path> for _Path {
    type Output = _Path;

    fn div(mut self, rhs: &Path) -> Self::Output {
        self.inner.extend(rhs);
        return self;
    }
}

impl ops::Div<&_Path> for &_Path {
    type Output = _Path;

    fn div(self, rhs: &_Path) -> Self::Output {
        let mut z = self.inner.to_owned();
        z.extend(&rhs.inner);
        return _Path {
            inner: z
        };
    }
}

impl ops::Div<&Path> for &_Path {
    type Output = _Path;

    fn div(self, rhs: &Path) -> Self::Output {
        let mut z = self.inner.to_owned();
        z.extend(rhs);
        return _Path {
            inner: z
        };
    }
}

impl std::fmt::Display for _Path {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}
