{
  description = "openclaw-marmot dev shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    moq.url = "github:moq-dev/moq/f90528efe324cf9d8c2de0f998f9bc9ac8aabda2";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      moq,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          packages =
            (with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              just
              docker
              docker-compose
            ])
            ++ [
              moq.packages.${system}.moq-relay
            ];
        };
      }
    );
}
