# Built in Copr via Packit: Packit replaces Source0 with the archive it
# generates from the release tag (see packit.yaml create-archive), so the
# Source0 URL below is only a fallback for manual rpmbuild use.
Name:           gifdeck
Version:        0.1.0
Release:        1%{?dist}
Summary:        GIF picker for the terminal: search, favorite, clipboard

License:        GPL-3.0-only
URL:            https://github.com/oneshinyboi/gifdeck
Source0:        %{url}/archive/refs/tags/v%{version}/gifdeck-%{version}.tar.gz

BuildRequires:  rust
BuildRequires:  cargo
BuildRequires:  cmake
BuildRequires:  gcc
BuildRequires:  gcc-c++

%global _description %{expand:
A GIF picker for the terminal. Search GIPHY and KLIPY, browse results in
an animated grid, favorite the ones you love, and send them to the
clipboard — as the actual animated GIF file, or just as a link.}

%description
%{_description}

%prep
%autosetup

%build
cargo build --release

%install
install -Dpm 0755 target/release/gifdeck %{buildroot}%{_bindir}/gifdeck

%files
%license LICENSE
%doc README.md
%{_bindir}/gifdeck

%changelog
%autochangelog
