use aspotify::{
	Album, Artist, Client, ClientCredentials, ItemType, Market, Playlist, PlaylistItemType, Track,
	TrackSimplified,
};
use librespot::core::authentication::Credentials;
use librespot::core::cache::Cache;
use librespot::core::config::SessionConfig;
use librespot::core::session::Session;
use librespot::oauth::OAuthClientBuilder;
use std::fmt;
use url::Url;

use crate::error::SpotifyError;
use crate::settings::{Settings, get_credentials_cache_path};

/// Redirect URI already registered to the desktop client ID that `SessionConfig::default()` uses
/// A Developer Dashboard app cannot request the `streaming` scope, so login uses that official client
const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

pub struct Spotify {
	// librespotify sessopm
	pub session: Session,
	pub spotify: Client,
	pub market: Option<Market>,
}

impl Spotify {
	/// Create a streaming session plus a Web API client
	///
	/// Streaming login prefers reusable credentials in the config directory.
	/// If those are missing or rejected, a stored refresh token or a browser OAuth login is used.
	/// `client_id` / `client_secret` stay with aspotify for metadata; they cannot mint streaming tokens.
	pub async fn new(settings: &mut Settings) -> Result<Spotify, SpotifyError> {
		let session = Self::connect_session(settings).await?;

		let credentials = ClientCredentials {
			id: settings.client_id.clone(),
			secret: settings.client_secret.clone(),
		};
		let spotify = Client::new(credentials);

		Ok(Spotify {
			session,
			spotify,
			market: settings.market_country_code.map(Market::Country),
		})
	}

	/// Open a librespot session, refreshing or prompting for login when cached credentials are gone
	async fn connect_session(settings: &mut Settings) -> Result<Session, SpotifyError> {
		let cache = Self::open_credentials_cache()?;

		// Reusable credentials from a previous login outlive the one-hour OAuth access token
		if let Some(credentials) = cache.credentials() {
			let session = Session::new(SessionConfig::default(), Some(cache));
			match session.connect(credentials, true).await {
				Ok(()) => return Ok(session),
				Err(e) => {
					warn!("Cached credentials were rejected: {e}. Requesting a new login.");
				}
			}
		}

		if let Some(access_token) = Self::access_token_from_refresh_token(settings).await {
			if let Ok(session) = Self::connect_with_token(&access_token).await {
				return Ok(session);
			}
			warn!("Refresh token was rejected. Opening a browser for Spotify login.");
		}

		let access_token = Self::login_with_browser(settings).await?;
		Self::connect_with_token(&access_token).await
	}

	fn open_credentials_cache() -> Result<Cache, SpotifyError> {
		let cache_path = get_credentials_cache_path();
		if let Some(parent) = cache_path.parent() {
			std::fs::create_dir_all(parent)?;
		}
		Cache::new(Some(cache_path), None, None, None)
			.map_err(|e| SpotifyError::Error(e.to_string()))
	}

	/// Login once with a short-lived OAuth access token so librespot can write reusable credentials
	async fn connect_with_token(access_token: &str) -> Result<Session, SpotifyError> {
		let cache = Self::open_credentials_cache()?;
		let session = Session::new(SessionConfig::default(), Some(cache));
		session
			.connect(Credentials::with_access_token(access_token), true)
			.await?;
		Ok(session)
	}

	/// Exchange the stored refresh token for a short-lived streaming access token
	async fn access_token_from_refresh_token(settings: &mut Settings) -> Option<String> {
		let refresh_token = usable_token(&settings.refresh_token)?.to_string();
		let client = match oauth_client() {
			Ok(client) => client,
			Err(e) => {
				warn!("Could not build the OAuth client used to refresh the token: {e}");
				return None;
			}
		};

		match client.refresh_token_async(&refresh_token).await {
			Ok(token) => {
				if !token.refresh_token.is_empty() {
					settings.refresh_token = token.refresh_token;
				}
				Some(token.access_token)
			}
			Err(e) => {
				warn!("Could not refresh the Spotify access token: {e}");
				None
			}
		}
	}

	/// Authorization-code login in the default browser, then persist the returned refresh token
	async fn login_with_browser(settings: &mut Settings) -> Result<String, SpotifyError> {
		println!("Opening a browser for Spotify login. Return here after authorizing the app.");
		let token = oauth_client()?.get_access_token_async().await?;
		if !token.refresh_token.is_empty() {
			settings.refresh_token = token.refresh_token;
		}
		Ok(token.access_token)
	}

	/// Parse URI or URL into URI
	pub fn parse_uri(uri: &str) -> Result<String, SpotifyError> {
		// Already URI
		if uri.starts_with("spotify:") {
			if uri.split(':').count() < 3 {
				return Err(SpotifyError::InvalidUri);
			}
			return Ok(uri.to_string());
		}

		// Parse URL
		let url = Url::parse(uri)?;
		// Track / album / playlist / artist share links use this host
		if url.host_str() == Some("open.spotify.com") {
			let mut path = url
				.path_segments()
				.ok_or_else(|| SpotifyError::Error("Missing URL path".into()))?
				.peekable();
			// Localized share links prefix the item type and ID with an intl-* segment
			path.next_if(|segment| segment.starts_with("intl-"));
			let path = path.collect::<Vec<&str>>();
			if path.len() < 2 {
				return Err(SpotifyError::InvalidUri);
			}
			return Ok(format!("spotify:{}:{}", path[0], path[1]));
		}
		Err(SpotifyError::InvalidUri)
	}

	/// Fetch data for URI
	pub async fn resolve_uri(&self, uri: &str) -> Result<SpotifyItem, SpotifyError> {
		let parts = uri.split(':').skip(1).collect::<Vec<&str>>();
		let id = parts[1];
		match parts[0] {
			"track" => {
				let track = self.spotify.tracks().get_track(id, self.market).await?;
				Ok(SpotifyItem::Track(track.data))
			}
			"playlist" => {
				let playlist = self
					.spotify
					.playlists()
					.get_playlist(id, self.market)
					.await?;
				Ok(SpotifyItem::Playlist(playlist.data))
			}
			"album" => {
				let album = self.spotify.albums().get_album(id, self.market).await?;
				Ok(SpotifyItem::Album(album.data))
			}
			"artist" => {
				let artist = self.spotify.artists().get_artist(id).await?;
				Ok(SpotifyItem::Artist(artist.data))
			}
			// Unsupported / Unimplemented
			_ => Ok(SpotifyItem::Other(uri.to_string())),
		}
	}

	/// Get search results for query
	pub async fn search(&self, query: &str) -> Result<Vec<Track>, SpotifyError> {
		Ok(self
			.spotify
			.search()
			.search(query, [ItemType::Track], true, 50, 0, None)
			.await?
			.data
			.tracks
			.unwrap()
			.items)
	}

	/// Get all tracks from playlist
	pub async fn full_playlist(&self, id: &str) -> Result<Vec<Track>, SpotifyError> {
		let mut items = vec![];
		let mut offset = 0;
		loop {
			let page = self
				.spotify
				.playlists()
				.get_playlists_items(id, 100, offset, self.market)
				.await?;
			items.append(
				&mut page
					.data
					.items
					.iter()
					.filter_map(|i| -> Option<Track> {
						if let Some(PlaylistItemType::Track(t)) = &i.item {
							Some(t.to_owned())
						} else {
							None
						}
					})
					.collect(),
			);

			// End
			offset += page.data.items.len();
			if page.data.total == offset {
				return Ok(items);
			}
		}
	}

	/// Get all tracks from album
	pub async fn full_album(&self, id: &str) -> Result<Vec<TrackSimplified>, SpotifyError> {
		let mut items = vec![];
		let mut offset = 0;
		loop {
			let page = self
				.spotify
				.albums()
				.get_album_tracks(id, 50, offset, self.market)
				.await?;
			items.append(&mut page.data.items.to_vec());

			// End
			offset += page.data.items.len();
			if page.data.total == offset {
				return Ok(items);
			}
		}
	}

	/// Get all tracks from artist
	pub async fn full_artist(&self, id: &str) -> Result<Vec<TrackSimplified>, SpotifyError> {
		let mut items = vec![];
		let mut offset = 0;
		loop {
			let page = self
				.spotify
				.artists()
				.get_artist_albums(id, None, 50, offset, self.market)
				.await?;

			for album in &mut page.data.items.iter() {
				items.append(&mut self.full_album(&album.id).await?)
			}

			// End
			offset += page.data.items.len();
			if page.data.total == offset {
				return Ok(items);
			}
		}
	}
}

/// Placeholder values left in the default settings file are not usable tokens
fn usable_token(value: &str) -> Option<&str> {
	let trimmed = value.trim();
	if trimmed.is_empty() || trimmed == "refresh_token" {
		None
	} else {
		Some(trimmed)
	}
}

/// Build the PKCE client that librespot uses for the official desktop client ID
fn oauth_client() -> Result<librespot::oauth::OAuthClient, SpotifyError> {
	// The session later talks to Spotify's internal APIs, so the token must come from this client ID
	let client_id = SessionConfig::default().client_id;
	Ok(
		OAuthClientBuilder::new(&client_id, OAUTH_REDIRECT_URI, vec!["streaming"])
			.open_in_browser()
			.build()?,
	)
}

impl Clone for Spotify {
	fn clone(&self) -> Self {
		Self {
			session: self.session.clone(),
			spotify: Client::new(self.spotify.credentials.clone()),
			market: self.market,
		}
	}
}

/// Basic debug implementation so can be used in other structs
impl fmt::Debug for Spotify {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "<Spotify Instance>")
	}
}

#[derive(Debug, Clone)]
pub enum SpotifyItem {
	Track(Track),
	Album(Album),
	Playlist(Playlist),
	Artist(Artist),
	/// Unimplemented
	Other(String),
}
