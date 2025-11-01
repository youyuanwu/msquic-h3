cmake_minimum_required(VERSION 3.16)

project(msquic-asio LANGUAGES)

if(WIN32)
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