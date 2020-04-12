/// Connection Phase Packets
/// https://dev.mysql.com/doc/internals/en/connection-phase-packets.html#packet-Protocol::HandshakeResponse41
/// start      length           value
/// 0           4           capability flags
/// 4           4           max-packet size
/// 8           1           character set
/// 9           23          reserved (all [0])
/// 32          unknown     user name
/// unknown     unknown     (auth response length) auth response
/// unknown     unknown     database
/// unknown     unknown     plugin name

use crate::constants::CapabilityFlag;
use byteorder::{WriteBytesExt, LittleEndian, ReadBytesExt};
use std::io::{Write, Cursor, Read, BufRead};
use sha1::{Sha1, Digest};
use crate::constants::MYSQL_NATIVE_PASSWORD;
use crate::errors::{ProtoResult, ProtoError};
use std::{io, cmp, convert};

#[derive(Debug, Clone, Default)]
pub struct Auth {
    character_set: u8,
    max_packet_size: u32,
    capability_flags: u32,
    auth_response: Vec<u8>,
    plugin_name: String,
    database: String,
    user: String,
}

/// Remove the boundary value that we don't want.
/// e.g. 0x00
pub trait ReadUntil: io::BufRead {
    fn real_read_until(&mut self, byte: u8, buf: &mut Vec<u8>) -> io::Result<usize>;
}

impl<T: convert::AsRef<[u8]>> ReadUntil for Cursor<T> {
    fn real_read_until(&mut self, byte: u8, buf: &mut Vec<u8>) -> io::Result<usize> {
        let size = self.read_until(byte, buf)?;
        if buf.len() > 0 {
            buf.remove(buf.len() - 1);
        }
        Ok(size)
    }
}

impl Auth {
    pub fn new() -> Self {
        Auth {
            character_set: 0,
            max_packet_size: 0,
            auth_response: vec![],
            capability_flags: 0,
            plugin_name: "".to_string(),
            database: "".to_string(),
            user: "".to_string(),
        }
    }

    pub fn charset(&self) -> u8 {
        self.character_set
    }

    pub fn auth_response(&self) -> &Vec<u8> {
        &self.auth_response
    }

    pub fn database(&self) -> &String {
        &self.database
    }

    pub fn user(&self) -> &String {
        &self.user
    }

    pub fn clean_resp(&mut self) {
        self.auth_response.clear()
    }

    pub fn write_handshake_resp(mut capability_flag: u32, charset: u8, username: String, password: String, salt: &[u8], database: String) -> Vec<u8> {
        if database.len() > 0 {
            capability_flag |= CapabilityFlag::CapabilityClientConnectWithDB as u32;
        } else {
            capability_flag &= !(CapabilityFlag::CapabilityClientConnectWithDB as u32);
        }
        let mut buf = vec![];
        buf.write_u32::<LittleEndian>(capability_flag)
            .expect("Unable to write");
        buf.write_u32::<LittleEndian>(0)
            .expect("Unable to write");
        // charset
        buf.write_u8(charset)
            .expect("Unable to write");

        buf.write_all(&[0; 23]).expect("Unable to write");
        // username
        buf.write_all(username.as_bytes())
            .expect("Unable to write");
        buf.write(&[0; 1]).expect("Unable to write");

        let auth_resp = gen_native_password(password, &salt);
        if (capability_flag & CapabilityFlag::CapabilityClientSecureConnection as u32) > 0 {
            buf.write_u8(auth_resp.len() as u8);
            buf.write_all(auth_resp.as_slice());
        } else {
            buf.write_all(auth_resp.as_slice());
            buf.write_u8(0).expect("Unable to write");
        }
        capability_flag &= !(CapabilityFlag::CapabilityClientPluginAuthLenencClientData as u32);
        if (capability_flag & CapabilityFlag::CapabilityClientConnectWithDB as u32) > 0 {
            buf.write_all(database.as_bytes());
            buf.write_u8(0).expect("Unable to write");
        }
        buf.write_all(MYSQL_NATIVE_PASSWORD.as_bytes());
        buf.write_u8(0).expect("Unable to write");
        buf
    }

    pub fn parse_handshake(&mut self, payload: &[u8]) -> ProtoResult<()> {
        let mut payload = Cursor::new(payload);
        // Parse client flag
        match payload.read_u32::<LittleEndian>() {
            Ok(client_flag) => {
                if client_flag & CapabilityFlag::CapabilityClientProtocol41 as u32 == 0 {
                    return Err(ProtoError::ProtocolNotSupport);
                }
                self.capability_flags = client_flag;
            }
            Err(_) => {
                return Err(ProtoError::ReadClientFlagError);
            }
        }
        // Parse max packet size
        self.max_packet_size = payload.read_u32::<LittleEndian>()
            .map_err(|_| { ProtoError::ReadMaxPacketSizeError })?;
        // Parse charset
        self.character_set = payload.read_u8()
            .map_err(|_| { ProtoError::ReadCharsetError })?;
        // Read 23 zeros
        let mut trailer = [0; 23];
        if payload.read(&mut trailer).map_err(|_| { ProtoError::ReadZeroError })? != trailer.len() {
            return Err(ProtoError::ReadZeroError);
        }
        unsafe {
            // Parse user name
            payload.real_read_until(0x00, self.user.as_mut_vec())
                .map_err(|_| { ProtoError::ReadUserError })?;
            // Parse auth response
            let mut auth_resp_len = 0;
            if (self.capability_flags & CapabilityFlag::CapabilityClientSecureConnection as u32) != 0 {
                auth_resp_len = payload.read_u8()
                    .map_err(|_| { ProtoError::ReadAuthResponseLengthError })? as usize;

                let mut buffer = [0; 256];
                payload.read(&mut buffer[..auth_resp_len])
                    .map_err(|_| { ProtoError::ReadAuthResponseError })?;
                self.auth_response.extend_from_slice(&buffer[..auth_resp_len]);
            } else {
                let mut buffer = [0; 20];
                payload.read(&mut buffer);
                self.auth_response.extend_from_slice(&buffer);
                payload.read_u8().map_err(|_| { ProtoError::ReadAuthResponseError })?;
            }
            // Parse database
            if (self.capability_flags & CapabilityFlag::CapabilityClientConnectWithDB as u32) > 0 {
                payload.real_read_until(0x00, self.database.as_mut_vec())
                    .map_err(|_| { ProtoError::ReadDatabaseError })?;
            }
            // Parse plugin name
            if (self.capability_flags & CapabilityFlag::CapabilityClientPluginAuth as u32) > 0 {
                payload.real_read_until(0x00, self.plugin_name.as_mut_vec())
                    .map_err(|_| { ProtoError::ReadPluginError })?;
            }
            // Also check the value order.
            if self.plugin_name != String::from(MYSQL_NATIVE_PASSWORD) {
                return Err(ProtoError::InvalidPluginError(self.plugin_name.clone()));
            }
        }
        Ok(())
    }
}

/// https://dev.mysql.com/doc/internals/en/secure-password-authentication.html#packet-Authentication::Native41
fn gen_native_password(password: String, salt: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return vec![];
    }
    let mut hasher = Sha1::new();
    hasher.input(password.as_bytes());
    let stage1 = hasher.result_reset();

    hasher.input(&stage1);
    let stage1_sha1 = hasher.result_reset();

    hasher.input(salt);
    hasher.input(stage1_sha1);
    let stage2 = hasher.result();

    let mut scramble = vec![0; stage2.len()];
    for index in 0..stage2.len() {
        scramble[index] = stage1[index] ^ stage2[index];
    }
    scramble
}

impl cmp::PartialEq for Auth {
    fn eq(&self, other: &Self) -> bool {
        self.plugin_name == other.plugin_name
            && self.database == other.database
            && self.capability_flags == other.capability_flags
            && self.character_set == other.character_set
            && self.max_packet_size == other.max_packet_size
            && self.auth_response == other.auth_response
            && self.user == other.user
    }
}

#[cfg(test)]
mod tests {
    use crate::proto::Auth;
    use crate::constants::{MYSQL_NATIVE_PASSWORD, DEFAULT_CLIENT_CAPABILITY, DEFAULT_SERVER_CAPABILITY, DEFAULT_SALT};
    use crate::errors::ProtoError;
    use crate::proto::auth::gen_native_password;
    use crate::constants::CapabilityFlag;

    #[test]
    fn test_auth() {
        let data = &[
            0x8d, 0xa6, 0xff, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x72, 0x6f, 0x6f, 0x74, 0x00, 0x14, 0x0e, 0xb4,
            0xdd, 0xb5, 0x5b, 0x64, 0xf8, 0x54, 0x40, 0xfd,
            0xf3, 0x45, 0xfa, 0x37, 0x12, 0x20, 0x20, 0xda,
            0x38, 0xaa, 0x61, 0x62, 0x63, 0x00, 0x6d, 0x79,
            0x73, 0x71, 0x6c, 0x5f, 0x6e, 0x61, 0x74, 0x69,
            0x76, 0x65, 0x5f, 0x70, 0x61, 0x73, 0x73, 0x77,
            0x6f, 0x72, 0x64, 0x00];
        let mut auth = Auth::new();
        auth.parse_handshake(data).unwrap();
        assert_eq!(auth.character_set, 33);
        assert_eq!(auth.max_packet_size, 16777216);
        assert_eq!(auth.capability_flags, 33531533);
        assert_eq!(auth.plugin_name, String::from(MYSQL_NATIVE_PASSWORD));
        assert_eq!(auth.database, "abc".to_string());
        assert_eq!(auth.user, "root".to_string());
        assert_eq!(auth.auth_response, vec![
            0x0e, 0xb4, 0xdd, 0xb5, 0x5b, 0x64, 0xf8, 0x54,
            0x40, 0xfd, 0xf3, 0x45, 0xfa, 0x37, 0x12, 0x20,
            0x20, 0xda, 0x38, 0xaa]);
    }

    #[test]
    fn test_error() {
        let data = &[0x8d, 0xa6, 0xff];
        let mut auth = Auth::new();
        match auth.parse_handshake(data) {
            Err(ProtoError::ReadClientFlagError) => {}
            _ => { panic!("Unexpected result"); }
        }
        let data = &[0x8d, 0xa6, 0xff, 0x01, 0x00, 0x00, 0x00];
        let mut auth = Auth::new();
        match auth.parse_handshake(data) {
            Err(ProtoError::ReadMaxPacketSizeError) => {}
            _ => { panic!("Unexpected result"); }
        }
        let data = &[0x8d, 0xa6, 0xff, 0x01, 0x00, 0x00, 0x00, 0x01];
        let mut auth = Auth::new();
        match auth.parse_handshake(data) {
            Err(ProtoError::ReadCharsetError) => {}
            _ => { panic!("Unexpected result"); }
        }
        let data = &[0x8d, 0xa6, 0xff, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x21, 0x00, 0x00, 0x00];
        let mut auth = Auth::new();
        match auth.parse_handshake(data) {
            Err(ProtoError::ReadZeroError) => {}
            _ => { panic!("Unexpected result"); }
        }
    }

    #[test]
    fn test_unpack() {
        let mut expected = Auth::new();
        expected.character_set = 0x02;
        expected.capability_flags = DEFAULT_CLIENT_CAPABILITY | CapabilityFlag::CapabilityClientConnectWithDB as u32;
        expected.auth_response = gen_native_password(String::from("password"), DEFAULT_SALT);
        expected.database = "test_db".to_string();
        expected.user = "root".to_string();
        expected.plugin_name = MYSQL_NATIVE_PASSWORD.to_string();

        let mut actual = Auth::new();
        let tmp = Auth::write_handshake_resp(
            DEFAULT_CLIENT_CAPABILITY,
            0x02,
            "root".to_string(),
            "password".to_string(),
            DEFAULT_SALT,
            "test_db".to_string(),
        );
        actual.parse_handshake(tmp.as_slice()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_unpack_with_empty_db() {
        let mut expected = Auth::new();
        expected.character_set = 0x02;
        expected.capability_flags = DEFAULT_CLIENT_CAPABILITY;
        expected.auth_response = gen_native_password(String::from("password"), DEFAULT_SALT);
        expected.database = "".to_string();
        expected.user = "root".to_string();
        expected.plugin_name = MYSQL_NATIVE_PASSWORD.to_string();

        let mut actual = Auth::new();
        let tmp = Auth::write_handshake_resp(
            DEFAULT_CLIENT_CAPABILITY,
            0x02,
            "root".to_string(),
            "password".to_string(),
            DEFAULT_SALT,
            "".to_string(),
        );
        actual.parse_handshake(tmp.as_slice()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_unpack_without_pwd() {
        let mut expected = Auth::new();
        expected.character_set = 0x02;
        expected.capability_flags = DEFAULT_CLIENT_CAPABILITY | CapabilityFlag::CapabilityClientConnectWithDB as u32;
        expected.auth_response = gen_native_password(String::from(""), DEFAULT_SALT);
        expected.database = "db".to_string();
        expected.user = "root".to_string();
        expected.plugin_name = MYSQL_NATIVE_PASSWORD.to_string();

        let mut actual = Auth::new();
        let tmp = Auth::write_handshake_resp(
            DEFAULT_CLIENT_CAPABILITY,
            0x02,
            "root".to_string(),
            "".to_string(),
            DEFAULT_SALT,
            "db".to_string(),
        );
        actual.parse_handshake(tmp.as_slice()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_unpack_without_secure() {
        let mut expected = Auth::new();
        expected.character_set = 0x02;
        expected.capability_flags = DEFAULT_CLIENT_CAPABILITY & !(CapabilityFlag::CapabilityClientSecureConnection as u32) & !(CapabilityFlag::CapabilityClientPluginAuthLenencClientData as u32);
        expected.capability_flags |= CapabilityFlag::CapabilityClientConnectWithDB as u32;
        expected.auth_response = gen_native_password(String::from("password"), DEFAULT_SALT);
        expected.database = "test_db".to_string();
        expected.user = "root".to_string();
        expected.plugin_name = MYSQL_NATIVE_PASSWORD.to_string();

        let mut actual = Auth::new();
        let tmp = Auth::write_handshake_resp(
            DEFAULT_CLIENT_CAPABILITY & !(CapabilityFlag::CapabilityClientSecureConnection as u32),
            0x02,
            "root".to_string(),
            "password".to_string(),
            DEFAULT_SALT,
            "test_db".to_string(),
        );
        actual.parse_handshake(tmp.as_slice()).unwrap();
        assert_eq!(actual, expected);
    }
}