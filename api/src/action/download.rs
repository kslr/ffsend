use std::fs::File;
use std::io::{
    self,
    Error as IoError,
    Read,
};
use std::sync::{Arc, Mutex};

use openssl::symm::decrypt_aead;
use reqwest::{
    Client, 
    Error as ReqwestError,
    Response,
};
use reqwest::header::Authorization;
use reqwest::header::ContentLength;
use serde_json;

use crypto::b64;
use crypto::key_set::KeySet;
use crypto::sign::signature_encoded;
use file::file::DownloadFile;
use file::metadata::Metadata;
use reader::{EncryptedFileWriter, ProgressReporter, ProgressWriter};

pub type Result<T> = ::std::result::Result<T, DownloadError>;
type StdResult<T, E> = ::std::result::Result<T, E>;

/// The name of the header that is used for the authentication nonce.
const HEADER_AUTH_NONCE: &'static str = "WWW-Authenticate";

// TODO: experiment with `iv` of `None` in decrypt logic

/// A file upload action to a Send server.
pub struct Download<'a> {
    /// The Send file to download.
    file: &'a DownloadFile,
}

impl<'a> Download<'a> {
    /// Construct a new download action for the given file.
    pub fn new(file: &'a DownloadFile) -> Self {
        Self {
            file,
        }
    }

    /// Invoke the download action.
    pub fn invoke(
        self,
        client: &Client,
        reporter: Arc<Mutex<ProgressReporter>>,
    ) -> Result<()> {
        // Create a key set for the file
        let mut key = KeySet::from(self.file);

        // Fetch the authentication nonce
        let auth_nonce = self.fetch_auth_nonce(client)
            .map_err(|err| DownloadError::AuthError(err))?;

        // Fetch the meta nonce, set the input vector
        let meta_nonce = self.fetch_meta_nonce(&client, &mut key, auth_nonce)
            .map_err(|err| DownloadError::MetaError(err))?;

        // Open the file we will write to
        // TODO: this should become a temporary file first
        let out = File::create("downloaded.zip")
            .map_err(|err| DownloadError::FileOpenError(err))?;

        // Create the file reader for downloading
        let (reader, len) = self.create_file_reader(&key, meta_nonce, &client);

        // Create the file writer
        let writer = self.create_file_writer(
            out,
            len,
            &key,
            reporter.clone(),
        );

        // Download the file
        self.download(reader, writer, len, reporter);

        // TODO: return the file path
        // TODO: return the new remote state (does it still exist remote)

        Ok(())
    }

    /// Fetch the authentication nonce for the file from the Send server.
    fn fetch_auth_nonce(&self, client: &Client)
        -> StdResult<Vec<u8>, AuthError>
    {
        // Get the download url, and parse the nonce
        let download_url = self.file.download_url(false);
        let response = client.get(download_url)
            .send()
            .map_err(|_| AuthError::NonceReqFail)?;

        // Validate the status code
        // TODO: allow redirects here?
        if !response.status().is_success() {
            return Err(AuthError::NonceReqStatusErr);
        }

        // Get the authentication nonce
        b64::decode(
            response.headers()
                .get_raw(HEADER_AUTH_NONCE)
                .ok_or(AuthError::MissingNonceHeader)?
                .one()
                .ok_or(AuthError::EmptyNonceHeader)
                .and_then(|line| String::from_utf8(line.to_vec())
                    .map_err(|_| AuthError::MalformedNonceHeader)
                )?
                .split_terminator(" ")
                .skip(1)
                .next()
                .ok_or(AuthError::MissingNonceHeader)?
        ).map_err(|_| AuthError::MalformedNonce)
    }

    /// Fetch the metadata nonce.
    /// This method also sets the input vector on the given key set,
    /// extracted from the metadata.
    ///
    /// The key set, along with the authentication nonce must be given.
    /// The meta nonce is returned.
    fn fetch_meta_nonce(
        &self,
        client: &Client,
        key: &mut KeySet,
        auth_nonce: Vec<u8>,
    ) -> StdResult<Vec<u8>, MetaError> {
        // Fetch the metadata and the nonce
        let (metadata, meta_nonce) = self.fetch_metadata(client, key, auth_nonce)?;

        // Set the input vector, and return the nonce
        key.set_iv(metadata.iv());
        Ok(meta_nonce)
    }

    /// Create a metadata nonce, and fetch the metadata for the file from the
    /// Send server.
    ///
    /// The key set, along with the authentication nonce must be given.
    ///
    /// The metadata, with the meta nonce is returned.
    fn fetch_metadata(
        &self,
        client: &Client,
        key: &KeySet,
        auth_nonce: Vec<u8>,
    ) -> StdResult<(Metadata, Vec<u8>), MetaError> {
        // Compute the cryptographic signature for authentication
        let sig = signature_encoded(key.auth_key().unwrap(), &auth_nonce)
            .map_err(|_| MetaError::ComputeSignatureFail)?;

        // Buidl the request, fetch the encrypted metadata
        let mut response = client.get(self.file.api_meta_url())
            .header(Authorization(
                format!("send-v1 {}", sig)
            ))
            .send()
            .map_err(|_| MetaError::NonceReqFail)?;

        // Validate the status code
        // TODO: allow redirects here?
        if !response.status().is_success() {
            return Err(MetaError::NonceReqStatusErr);
        }

        // Get the metadata nonce
        let nonce = b64::decode(
            response.headers()
                .get_raw(HEADER_AUTH_NONCE)
                .ok_or(MetaError::MissingNonceHeader)?
                .one()
                .ok_or(MetaError::EmptyNonceHeader)
                .and_then(|line| String::from_utf8(line.to_vec())
                    .map_err(|_| MetaError::MalformedNonceHeader)
                )?
                .split_terminator(" ")
                .skip(1)
                .next()
                .ok_or(MetaError::MissingNonceHeader)?
        ).map_err(|_| MetaError::MalformedNonce)?;

        // Parse the metadata response, and decrypt it
        Ok((
            response.json::<MetadataResponse>()
                .map_err(|_| MetaError::MalformedMetadata)?
                .decrypt_metadata(&key)
                .map_err(|_| MetaError::DecryptMetadataFail)?,
            nonce,
        ))
    }

    /// Make a download request, and create a reader that downloads the
    /// encrypted file.
    ///
    /// The response representing the file reader is returned along with the
    /// length of the reader content.
    fn create_file_reader(
        &self,
        key: &KeySet,
        meta_nonce: Vec<u8>,
        client: &Client,
    ) -> (Response, u64) {
        // Compute the cryptographic signature
        // TODO: use the metadata nonce here?
        // TODO: do not unwrap, return an error
        let sig = signature_encoded(key.auth_key().unwrap(), &meta_nonce)
            .expect("failed to compute file signature");

        // Build and send the download request
        // TODO: do not unwrap here, return error
        let response = client.get(self.file.api_download_url())
            .header(Authorization(
                format!("send-v1 {}", sig)
            ))
            .send()
            .expect("failed to fetch file, failed to send request");

        // Validate the status code
        // TODO: allow redirects here?
        if !response.status().is_success() {
            // TODO: return error here
            panic!("failed to fetch file, request status is not successful");
        }

        // Get the content length
        // TODO: make sure there is enough disk space
        let len = response.headers().get::<ContentLength>()
            .expect("failed to fetch file, missing content length header")
            .0;

        (response, len)
    }

    /// Create a file writer.
    ///
    /// This writer will will decrypt the input on the fly, and writes the
    /// decrypted data to the given file.
    fn create_file_writer(
        &self,
        file: File,
        len: u64,
        key: &KeySet,
        reporter: Arc<Mutex<ProgressReporter>>,
    ) -> ProgressWriter<EncryptedFileWriter> {
        // Build an encrypted writer
        let mut writer = ProgressWriter::new(
            EncryptedFileWriter::new(
                file,
                len as usize,
                KeySet::cipher(),
                key.file_key().unwrap(),
                key.iv(),
            ).expect("failed to create encrypted writer")
        ).expect("failed to create encrypted writer");

        // Set the reporter
        writer.set_reporter(reporter.clone());

        writer
    }

    /// Download the file from the reader, and write it to the writer.
    /// The length of the file must also be given.
    /// The status will be reported to the given progress reporter.
    fn download<R: Read>(
        &self,
        mut reader: R,
        mut writer: ProgressWriter<EncryptedFileWriter>,
        len: u64,
        reporter: Arc<Mutex<ProgressReporter>>,
    ) {
        // Start the writer
        reporter.lock()
            .expect("unable to start progress, failed to get lock")
            .start(len);

        // Write to the output file
        io::copy(&mut reader, &mut writer)
            .expect("failed to download and decrypt file");

        // Finish
        reporter.lock()
            .expect("unable to finish progress, failed to get lock")
            .finish();

        // Verify the writer
        // TODO: delete the file if verification failed, show a proper error
        assert!(writer.unwrap().verified(), "downloaded and decrypted file could not be verified");
    }
}

/// Errors that may occur in the upload action. 
#[derive(Debug)]
pub enum DownloadError {
    /// An authentication related error.
    AuthError(AuthError),

    /// An metadata related error.
    MetaError(MetaError),

    /// An error occurred while opening the file for writing.
    FileOpenError(IoError),

    /// The given file is not not an existing file.
    /// Maybe it is a directory, or maybe it doesn't exist.
    NotAFile,

    /// An error occurred while opening or reading a file.
    FileError,

    /// An error occurred while encrypting the file.
    EncryptionError,

    /// An error occurred while while processing the request.
    /// This also covers things like HTTP 404 errors.
    RequestError(ReqwestError),

    /// An error occurred while decoding the response data.
    DecodeError,
}

#[derive(Debug)]
pub enum AuthError {
    NonceReqFail,
    NonceReqStatusErr,
    MissingNonceHeader,
    EmptyNonceHeader,
    MalformedNonceHeader,
    MalformedNonce,
}

#[derive(Debug)]
pub enum MetaError {
    ComputeSignatureFail,
    NonceReqFail,
    NonceReqStatusErr,
    MissingNonceHeader,
    EmptyNonceHeader,
    MalformedNonceHeader,
    MalformedNonce,
    MalformedMetadata,
    DecryptMetadataFail,
}

/// The metadata response from the server, when fetching the data through
/// the API.
/// 
/// This metadata is required to successfully download and decrypt the
/// corresponding file.
#[derive(Debug, Deserialize)]
struct MetadataResponse {
    /// The encrypted metadata.
    #[serde(rename="metadata")]
    meta: String,
}

impl MetadataResponse {
    /// Get and decrypt the metadata, based on the raw data in this response.
    ///
    /// The decrypted data is verified using an included tag.
    /// If verification failed, an error is returned.
    // TODO: do not unwrap, return a proper error
    pub fn decrypt_metadata(&self, key_set: &KeySet) -> Result<Metadata> {
        // Decode the metadata
        let raw = b64::decode(&self.meta)
            .expect("failed to decode metadata from server");

        // Get the encrypted metadata, and it's tag
        let (encrypted, tag) = raw.split_at(raw.len() - 16);
        // TODO: is the tag length correct, remove assert if it is
        assert_eq!(tag.len(), 16);

        // Decrypt the metadata
        // TODO: do not unwrap, return an error
		let meta = decrypt_aead(
			KeySet::cipher(),
			key_set.meta_key().unwrap(),
			Some(key_set.iv()),
			&[],
			encrypted,
			&tag,
		).expect("failed to decrypt metadata, invalid tag?");

        // Parse the metadata, and return
        Ok(
            serde_json::from_slice(&meta)
                .expect("failed to parse decrypted metadata as JSON")
        )
    }
}