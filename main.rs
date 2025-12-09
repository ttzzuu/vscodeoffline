use anyhow::{Context, Result};
use clap::Parser;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, IsCa, SanType,
};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::signal;
use walkdir::WalkDir;
use warp::Filter;

const MIMIKRY_TAG: &str = "#mimikry-entry";
const CA_CERT_FILENAME: &str = "mimikry-ca.crt";
const SYSTEM_CERT_DIR: &str = "/usr/local/share/ca-certificates";
const NSS_DB_DIR: &str = ".pki/nssdb";

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Comma separated list of domains to fake (e.g. github.com,mysite.org)
    #[arg(index = 1)]
    domains: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    if { users::get_current_uid() } != 0 {
        return Err(anyhow::anyhow!("Root privileges required. Please run with sudo."));
    }

    let args = Args::parse();
    let domains: Vec<String> = args.domains.split(',').map(|s| s.trim().to_string()).collect();

    println!(">> Mimikry starting for domains: {:?}", domains);

    // 1. Clean up any previous run's mess just in case
    cleanup_system().ok();

    // 2. Generate CA and Leaf Certificates
    let (ca_cert_pem, _, leaf_cert_pem, leaf_key_pem) = generate_certs(&domains)?;

    // 3. Install Trust
    install_trust(&ca_cert_pem).context("Failed to install trust")?;

    // 4. Update /etc/hosts
    update_hosts(&domains).context("Failed to update /etc/hosts")?;

    // 5. Serve
    let routes = warp::path::full()
        .map(|path: warp::path::FullPath| path.as_str().to_string())
        .and_then(handle_request)
        .with(warp::log::custom(|info| {
            println!("Request: {} {}", info.method(), info.path());
        }));

    let server_http = warp::serve(routes.clone()).run(([0, 0, 0, 0], 80));

    let server_https = warp::serve(routes)
        .tls()
        .cert(&leaf_cert_pem)
        .key(&leaf_key_pem)
        .run(([0, 0, 0, 0], 443));

    println!(">> Server running on port 80 and 443. serving artifacts...");
    println!(">> Press Ctrl+C to shutdown.");

    tokio::select! {
        _ = server_http => {},
        _ = server_https => {},
        _ = signal::ctrl_c() => {
            println!("\n>> Shutdown signal received.");
        }
    }

    // 6. Cleanup
    cleanup_system()?;
    println!(">> System cleaned. Goodbye.");

    Ok(())
}

async fn handle_request(path: String) -> Result<impl warp::Reply, warp::Rejection> {
    // Extract filename from the end of the URL
    let filename = Path::new(&path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if filename.is_empty() {
        return Err(warp::reject::not_found());
    }

    // Define search paths
    let mut search_dirs = Vec::new();
    
    // Attempt to get the REAL user's home dir (since we are running as root)
    let real_user_home = get_real_user_home();
    
    if let Some(home) = &real_user_home {
        search_dirs.push(home.join("Downloads"));
    }
    
    // Add media (USB)
    search_dirs.push(PathBuf::from("/media"));

    // Add Env var
    if let Ok(asset_dir) = env::var("MIMIKRY_ASSET_DIR") {
        search_dirs.push(PathBuf::from(asset_dir));
    }

    println!("   Looking for artifact: '{}'", filename);

    for dir in search_dirs {
        if !dir.exists() { continue; }
        
        // Recursive search in these directories
        for entry in WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            if entry.file_name() == filename {
                let full_path = entry.path();
                println!("   Found at: {:?}", full_path);

                let mime = mime_guess::from_path(full_path).first_or_octet_stream();
                
                // Read file
                if let Ok(contents) = fs::read(full_path) {
                    return Ok(warp::reply::with_header(
                        contents,
                        "Content-Type",
                        mime.as_ref(),
                    ));
                }
            }
        }
    }

    Err(warp::reject::not_found())
}

// --- Certificate Logic ---

fn generate_certs(domains: &[String]) -> Result<(String, String, String, String)> {
    // 1. Create a Self-Signed CA
    let mut ca_params = CertificateParams::new(vec!["Mimikry Root CA".to_string()]);
    ca_params.distinguished_name.push(DnType::OrganizationName, "Mimikry Internal");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    
    let ca_cert = Certificate::from_params(ca_params)?;
    let ca_cert_pem = ca_cert.serialize_pem()?;
    let ca_key_pem = ca_cert.serialize_private_key_pem();

    // 2. Create Leaf Cert signed by CA
    let mut leaf_params = CertificateParams::new(domains.to_vec());
    let mut sans = vec![];
    for d in domains {
        sans.push(SanType::DnsName(d.clone()));
    }
    leaf_params.subject_alt_names = sans;
    
    let leaf_cert = Certificate::from_params(leaf_params)?;
    let leaf_cert_pem = leaf_cert.serialize_pem_with_signer(&ca_cert)?;
    let leaf_key_pem = leaf_cert.serialize_private_key_pem();

    Ok((ca_cert_pem, ca_key_pem, leaf_cert_pem, leaf_key_pem))
}

// --- System Trust Logic ---

fn install_trust(ca_pem: &str) -> Result<()> {
    // 1. System Store (Ubuntu)
    let sys_cert_path = Path::new(SYSTEM_CERT_DIR).join(CA_CERT_FILENAME);
    let mut file = File::create(&sys_cert_path)?;
    file.write_all(ca_pem.as_bytes())?;

    println!("   Updating system CA store...");
    let status = Command::new("update-ca-certificates").output()?;
    if !status.status.success() {
        return Err(anyhow::anyhow!("Failed to run update-ca-certificates"));
    }

    // 2. NSS DB (Chrome/VSCode)
    // We need to do this for the SUDO_USER, not root
    if let Some(home) = get_real_user_home() {
        let nss_db_path = home.join(NSS_DB_DIR);
        let nss_db_url = format!("sql:{}", nss_db_path.to_string_lossy());

        // We need a temp file for certutil
        let temp_ca_path = PathBuf::from("/tmp").join(CA_CERT_FILENAME);
        fs::write(&temp_ca_path, ca_pem)?;

        println!("   Importing to NSS DB at: {}", nss_db_url);
        
        // certutil -A -n "Mimikry CA" -t "C,," -i /tmp/mimikry-ca.crt -d sql:/home/user/.pki/nssdb
        let status = Command::new("certutil")
            .arg("-A")
            .arg("-n")
            .arg("Mimikry CA")
            .arg("-t")
            .arg("C,,")
            .arg("-i")
            .arg(&temp_ca_path)
            .arg("-d")
            .arg(&nss_db_url)
            .output();

        // It might fail if DB doesn't exist, we try our best.
        if let Ok(out) = status {
            if !out.status.success() {
                eprintln!("   Warning: certutil failed: {}", String::from_utf8_lossy(&out.stderr));
            }
        }
        
        let _ = fs::remove_file(temp_ca_path);
    }

    Ok(())
}

fn remove_trust() -> Result<()> {
    // 1. Remove from System
    let sys_cert_path = Path::new(SYSTEM_CERT_DIR).join(CA_CERT_FILENAME);
    if sys_cert_path.exists() {
        fs::remove_file(sys_cert_path)?;
    }
    // We verify strict "fresh" removal
    Command::new("update-ca-certificates")
        .arg("--fresh")
        .output()?;

    // 2. Remove from NSS DB
    if let Some(home) = get_real_user_home() {
        let nss_db_path = home.join(NSS_DB_DIR);
        let nss_db_url = format!("sql:{}", nss_db_path.to_string_lossy());

        Command::new("certutil")
            .arg("-D")
            .arg("-n")
            .arg("Mimikry CA")
            .arg("-d")
            .arg(nss_db_url)
            .output()
            .ok(); // Ignore errors if cert didn't exist
    }

    Ok(())
}

// --- Hosts File Logic ---

fn update_hosts(domains: &[String]) -> Result<()> {
    let hosts_path = "/etc/hosts";
    let mut file = OpenOptions::new()
        .write(true)
        .append(true)
        .open(hosts_path)?;

    for domain in domains {
        writeln!(file, "127.0.0.1 {} {}", domain, MIMIKRY_TAG)?;
    }
    
    println!("   Added {} domains to /etc/hosts", domains.len());
    Ok(())
}

fn cleanup_hosts() -> Result<()> {
    let hosts_path = "/etc/hosts";
    let file = File::open(hosts_path)?;
    let reader = BufReader::new(file);

    let mut lines: Vec<String> = Vec::new();
    let mut changed = false;

    for line in reader.lines() {
        let line = line?;
        if line.trim().ends_with(MIMIKRY_TAG) {
            changed = true;
            continue; // Skip our lines
        }
        lines.push(line);
    }

    if changed {
        let mut file = File::create(hosts_path)?;
        for line in lines {
            writeln!(file, "{}", line)?;
        }
        println!("   Cleaned /etc/hosts");
    }

    Ok(())
}

fn cleanup_system() -> Result<()> {
    cleanup_hosts()?;
    remove_trust()?;
    Ok(())
}

// --- Utils ---

fn get_real_user_home() -> Option<PathBuf> {
    // Because we run as sudo, $HOME is /root. We want the user who called sudo.
    env::var("SUDO_USER").ok().and_then(|username| {
        // Simple heuristic: linux homes are usually /home/username
        // A more robust way requires looking up /etc/passwd but this works 99% of time
        let path = PathBuf::from("/home").join(username);
        if path.exists() { Some(path) } else { None }
    })
}