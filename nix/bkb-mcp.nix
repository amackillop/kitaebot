{
  rustPlatform,
  fetchCrate,
  pkg-config,
  openssl,
  lib,
}:

# Bitcoin Knowledge Base MCP server (spec 22). Search/lookup tools over
# BIPs, BOLTs, bLIPs, LUDs, NUTs and surrounding discussions, backed by
# bitcoinknowledge.dev. Pure knowledge lookup: no side effects, safe
# for the read-only sub-agent sets.
rustPlatform.buildRustPackage rec {
  pname = "bkb-mcp";
  version = "0.2.1";

  src = fetchCrate {
    inherit pname version;
    hash = "sha256-5rErDnwm4FRAkRkdqW1UI9U6bl6Y45uXh5Y4CYlIhYw=";
  };

  cargoHash = "sha256-OAU8/dw8M6WvHUCWzahwVddq1pC6/aFRiQ5tgSVeX2k=";

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];

  # Runtime tool; skip the test suite to keep builds fast.
  doCheck = false;

  meta = with lib; {
    description = "MCP server for the Bitcoin Knowledge Base (bitcoinknowledge.dev)";
    homepage = "https://github.com/tnull/bitcoin-knowledge-base";
    license = licenses.mit;
    mainProgram = "bkb-mcp";
  };
}
