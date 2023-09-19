Summary: QCrosVM Support
Name: qcrosvm
Version: 1.0
Release: R1
Source0:	 %{name}-%{version}.tar.gz
BuildRequires:	make libtool gcc-g++ systemd-rpm-macros cargo libcap-devel rust
BuildRequires:  rust-packaging
%{?systemd_requires}
Requires: systemd

License: BSD-3-Clause-Clear & BSD-3-Clause

%description
QCrosVM Support.

%global debug_package %{nil}

%prep
%setup -q -n qcrosvm
mkdir -p %{_builddir}/external
mkdir -p %{_builddir}/vendor/qcom/opensource
mv %{_builddir}/qcrosvm %{_builddir}/vendor/qcom/opensource
mv %{_builddir}/rust %{_builddir}/external/
mv %{_builddir}/crosvm %{_builddir}/external/
mv %{_builddir}/minijail %{_builddir}/external/
ln -sf %{_builddir}/vendor/qcom/opensource/qcrosvm %{_builddir}/qcrosvm

%build
%cargo_build -a

%install
mkdir -p %{buildroot}%{_bindir}
mkdir -p %{buildroot}%{_unitdir}
install -m 0755 -t  %{buildroot}%{_bindir} %{_builddir}/vendor/qcom/opensource/qcrosvm/target/release/%{name}
install -DpZm 0644 qcrosvm.service %{buildroot}%{_unitdir}

%post
systemctl enable qcrosvm.service

%preun
%systemd_preun qcrosvm.service

%postun
%systemd_postun_with_restart qcrosvm.service

%files
%{_bindir}/qcrosvm
%{_unitdir}/qcrosvm.service
