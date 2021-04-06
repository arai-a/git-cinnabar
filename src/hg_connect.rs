/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::ffi::{c_void, CStr, CString};
use std::fs::File;
use std::io::Write;
use std::os::raw::{c_char, c_int};
use std::ptr;

use bstr::{BString, ByteSlice};
use itertools::Itertools;
use percent_encoding::percent_decode;
use sha1::{Digest, Sha1};
use url::Url;

use crate::hg_bundle::copy_bundle;
use crate::hg_connect_http::get_http_connection;
use crate::hg_connect_stdio::get_stdio_connection;
use crate::libcinnabar::send_buffer;
use crate::libgit::{strbuf, string_list};
use crate::oid::ObjectId;
use crate::store::HgChangesetId;
use crate::util::FromBytes;

#[allow(non_camel_case_types)]
#[repr(transparent)]
struct hg_connection(c_void);

pub struct OneHgArg<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

pub struct HgArgs<'a> {
    pub args: &'a [OneHgArg<'a>],
    pub extra_args: Option<&'a [OneHgArg<'a>]>,
}

#[macro_export]
macro_rules! args {
    ($($n:ident : $v:expr,)* $(*: $a:expr)?) => {
        HgArgs {
            args: $crate::args!(@args $($n:$v),*),
            extra_args: $crate::args!(@extra $($a)?),
        }
    };
    ($($n:ident : $v:expr),*) => { $crate::args!($($n:$v,)*) };
    (@args $($n:ident : $v:expr),*) => {&[
        $(OneHgArg { name: stringify!($n), value: $v }),*
    ]};
    (@extra) => { None };
    (@extra $a:expr) => { Some($a) };
}

#[derive(Default)]
pub struct HgCapabilities {
    capabilities: Vec<(BString, CString)>,
}

impl HgCapabilities {
    /* Split the list of capabilities a mercurial server returned. Also url-decode
     * the bundle2 value (TODO: in place). */
    pub fn new_from(buf: &[u8]) -> Self {
        let mut capabilities = Vec::new();
        for item in buf.split(|&b| b == b' ') {
            let (name, value) = match item.find_byte(b'=') {
                Some(off) => {
                    let (name, value) = item.split_at(off);
                    (name, &value[1..])
                }
                None => (item, &b""[..]),
            };
            capabilities.push((
                BString::from(name.to_owned()),
                if name == b"bundle2" {
                    CString::new(percent_decode(value).collect::<Vec<_>>()).unwrap()
                } else {
                    CString::new(value.to_owned()).unwrap()
                },
            ));
        }
        HgCapabilities { capabilities }
    }

    pub fn get_capability(&self, needle: &[u8]) -> Option<&CStr> {
        for (name, value) in self.capabilities.iter() {
            if name == needle {
                return Some(&value);
            }
        }
        None
    }
}

pub trait HgConnectionBase {
    fn get_capability(&self, _name: &[u8]) -> Option<&CStr> {
        None
    }
}

pub trait HgWireConnection: HgConnectionBase {
    fn simple_command(&mut self, response: &mut strbuf, command: &str, args: HgArgs);

    fn changegroup_command(&mut self, out: &mut (dyn Write + Send), command: &str, args: HgArgs);

    fn push_command(&mut self, response: &mut strbuf, input: File, command: &str, args: HgArgs);
}

pub trait HgConnection: HgConnectionBase {
    fn wire(&mut self) -> Option<&mut dyn HgWireConnection> {
        None
    }

    //TODO: eventually, we'll want a better API here, not filling a strbuf.
    fn listkeys(&mut self, _result: &mut strbuf, _namespace: &str) {
        unimplemented!();
    }

    fn getbundle(
        &mut self,
        _out: &mut (dyn Write + Send),
        _heads: &[HgChangesetId],
        _common: &[HgChangesetId],
        _bundle2caps: Option<&str>,
    ) {
        unimplemented!();
    }

    //TODO: eventually, we'll want a better API here, not filling a strbuf.
    fn lookup(&mut self, _result: &mut strbuf, _key: &str) {
        unimplemented!();
    }
}

impl HgConnectionBase for Box<dyn HgWireConnection> {
    fn get_capability(&self, name: &[u8]) -> Option<&CStr> {
        (**self).get_capability(name)
    }
}

impl HgConnection for Box<dyn HgWireConnection> {
    fn wire(&mut self) -> Option<&mut dyn HgWireConnection> {
        Some(&mut **self)
    }

    fn listkeys(&mut self, result: &mut strbuf, namespace: &str) {
        self.simple_command(result, "listkeys", args!(namespace: namespace))
    }

    fn getbundle(
        &mut self,
        out: &mut (dyn Write + Send),
        heads: &[HgChangesetId],
        common: &[HgChangesetId],
        bundle2caps: Option<&str>,
    ) {
        let mut args = Vec::new();
        let heads = heads.iter().join(" ");
        let common = common.iter().join(" ");
        args.push(OneHgArg {
            name: "heads",
            value: &heads,
        });
        args.push(OneHgArg {
            name: "common",
            value: &common,
        });
        if let Some(caps) = bundle2caps {
            if !caps.is_empty() {
                args.push(OneHgArg {
                    name: "bundlecaps",
                    value: caps,
                });
            }
        }
        self.changegroup_command(out, "getbundle", args!(*: &args[..]));
    }

    fn lookup(&mut self, result: &mut strbuf, key: &str) {
        self.simple_command(result, "lookup", args!(key: key))
    }
}

unsafe fn hg_connection_from_ffi<'a>(ptr: *mut hg_connection) -> &'a mut (dyn HgConnection + 'a) {
    &mut **(ptr as *mut Box<dyn HgConnection>).as_mut().unwrap()
}

#[no_mangle]
unsafe extern "C" fn hg_get_capability(
    conn: *mut hg_connection,
    name: *const c_char,
) -> *const c_char {
    hg_connection_from_ffi(conn)
        .get_capability(CStr::from_ptr(name.as_ref().unwrap()).to_bytes().as_bstr())
        .map(CStr::as_ptr)
        .unwrap_or(ptr::null())
}

#[no_mangle]
unsafe extern "C" fn hg_get_repo_state(
    conn: *mut hg_connection,
    branchmap: *mut strbuf,
    heads: *mut strbuf,
    bookmarks: *mut strbuf,
) {
    let conn = hg_connection_from_ffi(conn);
    let branchmap = branchmap.as_mut().unwrap();
    let heads = heads.as_mut().unwrap();
    let bookmarks = bookmarks.as_mut().unwrap();
    if conn.get_capability(b"batch".as_bstr()).is_none() {
        // TODO: when not batching, check for coherency
        // (see the cinnabar.remote_helper python module)
        let wire = conn.wire().unwrap();
        wire.simple_command(branchmap, "branchmap", args!());
        wire.simple_command(heads, "heads", args!());
        conn.listkeys(bookmarks, "bookmarks");
    } else {
        let mut out = strbuf::new();
        conn.wire().unwrap().simple_command(
            &mut out,
            "batch",
            args!(
                cmds: "branchmap ;heads ;listkeys namespace=bookmarks",
                *: &[]
            ),
        );
        let split = out.as_bytes().split(|&b| b == b';');
        for (out, buf) in Iterator::zip(
            split,
            &mut [Some(branchmap), Some(heads), Some(bookmarks), None],
        ) {
            let buf = buf.as_mut().unwrap();
            unescape_batched_output(out, buf);
        }
    }
}

fn unescape_batched_output(out: &[u8], buf: &mut strbuf) {
    // This will fail if `split` has more than 3 items.
    let mut start = 0;
    let mut out = out;
    loop {
        if let Some(colon) = out[start..].find_byte(b':') {
            let (before, after) = out.split_at(start + colon);
            let replace = match after.get(..2) {
                Some(b":e") => Some(b"="),
                Some(b":s") => Some(b";"),
                Some(b":o") => Some(b","),
                Some(b":c") => Some(b":"),
                // This is not supposed to happen, but just in case:
                // XXX: throw an error?
                _ => None,
            };
            if let Some(replace) = replace {
                buf.extend_from_slice(before);
                buf.extend_from_slice(replace);
                out = &after[2..];
                if out.is_empty() {
                    break;
                }
            } else {
                start += colon + 1;
            }
        } else {
            buf.extend_from_slice(out);
            break;
        }
    }
}

#[test]
fn test_unescape_batched_output() {
    let mut buf = strbuf::new();
    unescape_batched_output(b"", &mut buf);
    assert_eq!(buf.as_bytes().as_bstr(), b"".as_bstr());

    let mut buf = strbuf::new();
    unescape_batched_output(b"abc", &mut buf);
    assert_eq!(buf.as_bytes().as_bstr(), b"abc".as_bstr());

    let mut buf = strbuf::new();
    unescape_batched_output(b"abc:def", &mut buf);
    assert_eq!(buf.as_bytes().as_bstr(), b"abc:def".as_bstr());

    let mut buf = strbuf::new();
    unescape_batched_output(b"abc:def:", &mut buf);
    assert_eq!(buf.as_bytes().as_bstr(), b"abc:def:".as_bstr());

    let mut buf = strbuf::new();
    unescape_batched_output(b"abc:edef:", &mut buf);
    assert_eq!(buf.as_bytes().as_bstr(), b"abc=def:".as_bstr());

    let mut buf = strbuf::new();
    unescape_batched_output(b"abc:edef:c", &mut buf);
    assert_eq!(buf.as_bytes().as_bstr(), b"abc=def:".as_bstr());

    let mut buf = strbuf::new();
    unescape_batched_output(b"abc:edef:c:s:e:oz", &mut buf);
    assert_eq!(buf.as_bytes().as_bstr(), b"abc=def:;=,z".as_bstr());
}

#[no_mangle]
unsafe extern "C" fn do_known(conn: *mut hg_connection, args: *const string_list) {
    let conn = hg_connection_from_ffi(conn).wire().unwrap();
    let args = args.as_ref().unwrap();
    let nodes = args
        .iter()
        .map(HgChangesetId::from_bytes)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let nodes_str = nodes.iter().join(" ");
    let mut result = strbuf::new();
    conn.simple_command(
        &mut result,
        "known",
        args!(
            nodes: &nodes_str,
            *: &[]
        ),
    );
    send_buffer(&result);
}

#[no_mangle]
unsafe extern "C" fn hg_listkeys(
    conn: *mut hg_connection,
    result: *mut strbuf,
    namespace: *const c_char,
) {
    let conn = hg_connection_from_ffi(conn);
    conn.listkeys(
        result.as_mut().unwrap(),
        CStr::from_ptr(namespace.as_ref().unwrap())
            .to_str()
            .unwrap(),
    )
}

#[no_mangle]
unsafe extern "C" fn do_getbundle(conn: *mut hg_connection, args: *const string_list) {
    let conn = hg_connection_from_ffi(conn);
    let mut args = args.as_ref().unwrap().iter();

    let arg_list = |a: &[u8]| {
        if a.is_empty() {
            Vec::new()
        } else {
            a.split(|&b| b == b',')
                .map(|b| HgChangesetId::from_bytes(b).unwrap())
                .collect()
        }
    };

    let heads = args.next().map(arg_list).unwrap_or_else(Vec::new);
    let common = args.next().map(arg_list).unwrap_or_else(Vec::new);
    let bundle2caps = args.next().map(|b| b.to_str().unwrap());
    assert!(args.next().is_none());

    let mut out = crate::libc::FdFile::stdout();
    conn.getbundle(&mut out, &heads, &common, bundle2caps);
}

#[no_mangle]
unsafe extern "C" fn do_unbundle(conn: *mut hg_connection, args: *const string_list) {
    let conn = hg_connection_from_ffi(conn);
    let args = args.as_ref().unwrap();
    let args = args.iter().collect::<Vec<_>>();
    let heads_str = if args.is_empty() || args[..] == [b"force"] {
        hex::encode("force")
    } else {
        let mut heads = args.iter().map(|a| HgChangesetId::from_bytes(a).unwrap());
        if conn.get_capability(b"unbundlehash").is_none() {
            heads.join(" ")
        } else {
            let mut hash = Sha1::new();
            for h in heads.sorted().dedup() {
                hash.update(h.as_raw_bytes());
            }
            format!("{} {:x}", hex::encode("hashed"), hash.finalize())
        }
    };

    /* Neither the stdio nor the HTTP protocols can handle a stream for
     * push commands, so store the data as a temporary file. */
    //TODO: error checking
    let tempfile = tempfile::Builder::new()
        .prefix("hg-bundle-")
        .suffix(".hg")
        .rand_bytes(6)
        .tempfile()
        .unwrap();
    let (mut f, path) = tempfile.into_parts();
    copy_bundle(&mut crate::libc::File::stdin(), &mut f).unwrap();
    drop(f);

    let file = File::open(path).unwrap();
    let mut response = strbuf::new();
    conn.wire()
        .unwrap()
        .push_command(&mut response, file, "unbundle", args!(heads: &heads_str));
    send_buffer(&response);
}

#[no_mangle]
unsafe extern "C" fn hg_pushkey(
    conn: *mut hg_connection,
    response: *mut strbuf,
    namespace: *const c_char,
    key: *const c_char,
    old: *const c_char,
    new: *const c_char,
) {
    let conn = hg_connection_from_ffi(conn).wire().unwrap();
    //TODO: handle the response being a mix of return code and output
    conn.simple_command(
        response.as_mut().unwrap(),
        "pushkey",
        args!(
            namespace: CStr::from_ptr(namespace.as_ref().unwrap()).to_str().unwrap(),
            key: CStr::from_ptr(key.as_ref().unwrap()).to_str().unwrap(),
            old: CStr::from_ptr(old.as_ref().unwrap()).to_str().unwrap(),
            new: CStr::from_ptr(new.as_ref().unwrap()).to_str().unwrap(),
        ),
    );
}

#[no_mangle]
unsafe extern "C" fn hg_lookup(conn: *mut hg_connection, result: *mut strbuf, key: *const c_char) {
    let conn = hg_connection_from_ffi(conn);
    conn.lookup(
        result.as_mut().unwrap(),
        CStr::from_ptr(key.as_ref().unwrap()).to_str().unwrap(),
    );
}

#[no_mangle]
unsafe extern "C" fn hg_clonebundles(conn: *mut hg_connection, result: *mut strbuf) {
    let conn = hg_connection_from_ffi(conn).wire().unwrap();
    conn.simple_command(result.as_mut().unwrap(), "clonebundles", args!());
}

#[no_mangle]
unsafe extern "C" fn hg_cinnabarclone(conn: *mut hg_connection, result: *mut strbuf) {
    let conn = hg_connection_from_ffi(conn).wire().unwrap();
    conn.simple_command(result.as_mut().unwrap(), "cinnabarclone", args!());
}

pub fn get_connection(url: &Url, flags: c_int) -> Option<Box<dyn HgConnection>> {
    if ["http", "https"].contains(&url.scheme()) {
        get_http_connection(&url)
    } else if ["ssh", "file"].contains(&url.scheme()) {
        get_stdio_connection(&url, flags)
    } else {
        die!("protocol '{}' is not supported", url.scheme());
    }
}

#[no_mangle]
unsafe extern "C" fn hg_connect(url: *const c_char, flags: c_int) -> *mut hg_connection {
    let url = Url::parse(CStr::from_ptr(url).to_str().unwrap()).unwrap();
    let mut conn = if let Some(conn) = get_connection(&url, flags) {
        conn
    } else {
        return ptr::null_mut();
    };

    if conn.wire().is_none() {
        // For now the wire helper just sends the bundle to stdout.
        let mut out = crate::libc::FdFile::stdout();
        out.write_all(b"bundle\n").unwrap();
        conn.getbundle(&mut out, &[], &[], None);
        return ptr::null_mut();
    }

    const REQUIRED_CAPS: [&str; 5] = [
        "getbundle",
        "branchmap",
        "known",
        "pushkey",
        //TODO: defer to when pushing.
        "unbundle",
    ];

    for cap in &REQUIRED_CAPS {
        if conn.get_capability(cap.as_bytes()).is_none() {
            die!(
                "Mercurial repository doesn't support the required \"{}\" capability.",
                cap
            );
        }
    }

    Box::into_raw(Box::new(conn)) as *mut hg_connection
}

#[no_mangle]
unsafe extern "C" fn hg_finish_connect(conn: *mut hg_connection) {
    Box::from_raw((conn as *mut Box<dyn HgConnection>).as_mut().unwrap());
    // The box is dropped here, invalidating the *mut hg_connection.
}
