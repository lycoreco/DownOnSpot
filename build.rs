fn main() {
	#[cfg(windows)]
	{
		winres::WindowsResource::new().compile().unwrap();
	}

	// Homebrew installs libmp3lame outside the default Apple linker search path
	#[cfg(target_os = "macos")]
	{
		let homebrew_lib = std::env::var("HOMEBREW_PREFIX")
			.map(|prefix| format!("{prefix}/lib"))
			.unwrap_or_else(|_| {
				if cfg!(target_arch = "aarch64") {
					"/opt/homebrew/lib".to_string()
				} else {
					"/usr/local/lib".to_string()
				}
			});
		println!("cargo:rustc-link-search=native={homebrew_lib}");
	}
}
