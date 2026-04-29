// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ca_pem_path = out_dir.join("ca.pem");

    if ca_pem_path.exists() {
        return;
    }

    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "A2A Example CA");
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();

    let mut server_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    server_params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        )));
    server_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    let server_key = rcgen::KeyPair::generate().unwrap();
    let ca_issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let server_cert = server_params.signed_by(&server_key, &ca_issuer).unwrap();

    std::fs::write(&ca_pem_path, ca_cert.pem()).unwrap();
    std::fs::write(out_dir.join("server.pem"), server_cert.pem()).unwrap();
    std::fs::write(out_dir.join("server.key"), server_key.serialize_pem()).unwrap();

    println!("cargo::rerun-if-changed=build.rs");
}
