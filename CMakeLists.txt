cmake_minimum_required(VERSION 3.16)

project(msquic-asio LANGUAGES)

set(_msquic_version "v2.4.5")
set(_win_pkg_url "https://github.com/microsoft/msquic/releases/download/${_msquic_version}/msquic_windows_x64_Release_schannel.zip")
set(_linux_pkg_url "https://github.com/microsoft/msquic/releases/download/${_msquic_version}/msquic_linux_x64_Release_openssl.zip")
if(WIN32)
  # windows downloads release for winmd generation. And for sys2 import lib link.
  set(release_url ${_win_pkg_url})
message(STATUS "fetching msquic_release " ${release_url})
include(FetchContent)
FetchContent_Declare(
  msquic_release
  DOWNLOAD_EXTRACT_TIMESTAMP true
  URL ${release_url}
)
FetchContent_MakeAvailable(msquic_release)

find_program(cargo_exe
  NAMES cargo
  REQUIRED
)

# generate cert on windows:
find_program(pwsh_exe
  NAMES pwsh.exe
  REQUIRED
)

# get the test certificate
execute_process(COMMAND pwsh.exe -Command "Get-ChildItem Cert:\\CurrentUser\\My | Where-Object -Property FriendlyName -EQ -Value MsQuic-Test | Select-Object -ExpandProperty Thumbprint -First 1"
  OUTPUT_VARIABLE TEST_CERT_HASH
)

if(TEST_CERT_HASH STREQUAL "")
  message(Status "Generating TEST_CERT_HASH")
  execute_process(
    COMMAND pwsh.exe -Command "New-SelfSignedCertificate -DnsName $env:computername,localhost -FriendlyName MsQuic-Test -KeyUsageProperty Sign -KeyUsage DigitalSignature -CertStoreLocation cert:\\CurrentUser\\My -HashAlgorithm SHA256 -Provider \"Microsoft Software Key Storage Provider\" -KeyExportPolicy Exportable"
    COMMAND_ERROR_IS_FATAL ANY
  )
  execute_process(COMMAND pwsh.exe -Command "Get-ChildItem Cert:\\CurrentUser\\My | Where-Object -Property FriendlyName -EQ -Value MsQuic-Test | Select-Object -ExpandProperty Thumbprint -First 1"
    OUTPUT_VARIABLE TEST_CERT_HASH
    COMMAND_ERROR_IS_FATAL ANY
  )
endif()
  
message(STATUS "Using TEST_CERT_HASH ${TEST_CERT_HASH}")
endif(WIN32)

# add msquic as git deps
message(STATUS "fetching msquic")
include(FetchContent)
FetchContent_Declare(
  msquic
  GIT_REPOSITORY https://github.com/youyuanwu/msquic.git
  GIT_TAG        789b1b5eb25f867b3436547c68220b52bc2c1e4a
)

FetchContent_GetProperties(msquic)
if(NOT msquic_POPULATED)
  FetchContent_Populate(msquic)
endif()