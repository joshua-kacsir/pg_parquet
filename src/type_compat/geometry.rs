use std::{ffi::CString, ops::Deref};

use once_cell::sync::OnceCell;
use pgrx::{
    datum::UnboxDatum,
    pg_sys::{
        get_extension_oid, get_extension_schema, makeString, Anum_pg_type_oid, AsPgCStr, Datum,
        GetSysCacheOid, InvalidOid, LookupFuncName, Oid, OidFunctionCall1Coll,
        SysCacheIdentifier::TYPENAMENSP, BYTEAOID,
    },
    FromDatum, IntoDatum, PgList,
};

// we need to reset the postgis context at each copy start
static mut POSTGIS_CONTEXT: OnceCell<PostgisContext> = OnceCell::new();

fn get_postgis_context() -> &'static PostgisContext {
    unsafe {
        POSTGIS_CONTEXT
            .get()
            .expect("postgis context is not initialized")
    }
}

pub(crate) fn reset_postgis_context() {
    unsafe { POSTGIS_CONTEXT.take() };

    unsafe {
        POSTGIS_CONTEXT
            .set(PostgisContext::new())
            .expect("failed to reset postgis context")
    };
}

pub(crate) fn is_postgis_geometry_type(typoid: Oid) -> bool {
    if let Some(geometry_typoid) = get_postgis_context().geometry_typoid {
        return typoid == geometry_typoid;
    }

    false
}

#[derive(Debug, PartialEq, Clone)]
struct PostgisContext {
    geometry_typoid: Option<Oid>,
    st_asbinary_funcoid: Option<Oid>,
    st_geomfromwkb_funcoid: Option<Oid>,
}

impl PostgisContext {
    fn new() -> Self {
        let postgis_ext_oid = unsafe { get_extension_oid("postgis".as_pg_cstr(), true) };
        let postgis_ext_oid = if postgis_ext_oid == InvalidOid {
            None
        } else {
            Some(postgis_ext_oid)
        };

        let postgis_ext_schema_oid =
            postgis_ext_oid.map(|postgis_ext_oid| unsafe { get_extension_schema(postgis_ext_oid) });

        let st_asbinary_funcoid = postgis_ext_oid.map(|postgis_ext_oid| {
            Self::st_asbinary_funcoid(
                postgis_ext_oid,
                postgis_ext_schema_oid.expect("expected postgis is created"),
            )
        });

        let st_geomfromwkb_funcoid = postgis_ext_oid.map(|_| Self::st_geomfromwkb_funcoid());

        let geometry_typoid = postgis_ext_oid.map(|_| {
            Self::geometry_typoid(
                postgis_ext_oid.expect("expected postgis is created"),
                postgis_ext_schema_oid.expect("expected postgis is created"),
            )
        });

        Self {
            geometry_typoid,
            st_asbinary_funcoid,
            st_geomfromwkb_funcoid,
        }
    }

    fn st_asbinary_funcoid(postgis_ext_oid: Oid, postgis_ext_schema_oid: Oid) -> Oid {
        unsafe {
            let postgis_geometry_typoid =
                Self::geometry_typoid(postgis_ext_oid, postgis_ext_schema_oid);

            let function_name = makeString("st_asbinary".as_pg_cstr());
            let mut function_name_list = PgList::new();
            function_name_list.push(function_name);

            let mut arg_types = vec![postgis_geometry_typoid];

            LookupFuncName(
                function_name_list.as_ptr(),
                1,
                arg_types.as_mut_ptr(),
                false,
            )
        }
    }

    fn st_geomfromwkb_funcoid() -> Oid {
        unsafe {
            let function_name = makeString("st_geomfromwkb".as_pg_cstr());
            let mut function_name_list = PgList::new();
            function_name_list.push(function_name);

            let mut arg_types = vec![BYTEAOID];

            LookupFuncName(
                function_name_list.as_ptr(),
                1,
                arg_types.as_mut_ptr(),
                false,
            )
        }
    }

    fn geometry_typoid(postgis_ext_oid: Oid, postgis_ext_schema_oid: Oid) -> Oid {
        if postgis_ext_oid == InvalidOid {
            return InvalidOid;
        }

        let postgis_geometry_type_name = CString::new("geometry").expect("CString::new failed");

        let postgis_geometry_typoid = unsafe {
            GetSysCacheOid(
                TYPENAMENSP as _,
                Anum_pg_type_oid as _,
                postgis_geometry_type_name.into_datum().unwrap(),
                postgis_ext_schema_oid.into_datum().unwrap(),
                Datum::from(0), // not used key
                Datum::from(0), // not used key
            )
        };

        if postgis_geometry_typoid == InvalidOid {
            return InvalidOid;
        }

        postgis_geometry_typoid
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Geometry(pub(crate) Vec<u8>);

// we store Geometry as a WKB byte vector, and we allow it to be dereferenced as such
impl Deref for Geometry {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Vec<u8>> for Geometry {
    fn from(wkb: Vec<u8>) -> Self {
        Self(wkb)
    }
}

impl IntoDatum for Geometry {
    fn into_datum(self) -> Option<Datum> {
        let st_geomfromwkb_funcoid = get_postgis_context()
            .st_geomfromwkb_funcoid
            .expect("st_geomfromwkb_funcoid");

        let wkb_datum = self.0.into_datum().expect("cannot convert wkb to datum");

        Some(unsafe { OidFunctionCall1Coll(st_geomfromwkb_funcoid, InvalidOid, wkb_datum) })
    }

    fn type_oid() -> Oid {
        get_postgis_context()
            .geometry_typoid
            .expect("postgis context not initialized")
    }
}

impl FromDatum for Geometry {
    unsafe fn from_polymorphic_datum(datum: Datum, is_null: bool, _typoid: Oid) -> Option<Self>
    where
        Self: Sized,
    {
        if is_null {
            None
        } else {
            let st_asbinary_func_oid = get_postgis_context()
                .st_asbinary_funcoid
                .expect("st_asbinary_funcoid");

            let geom_datum = datum;

            let wkb_datum =
                unsafe { OidFunctionCall1Coll(st_asbinary_func_oid, InvalidOid, geom_datum) };

            let is_null = false;
            let wkb =
                Vec::<u8>::from_datum(wkb_datum, is_null).expect("cannot convert datum to wkb");
            Some(Self(wkb))
        }
    }
}

unsafe impl UnboxDatum for Geometry {
    type As<'src> = Geometry;

    unsafe fn unbox<'src>(datum: pgrx::datum::Datum<'src>) -> Self::As<'src>
    where
        Self: 'src,
    {
        let st_asbinary_func_oid = get_postgis_context()
            .st_asbinary_funcoid
            .expect("st_asbinary_funcoid");

        let geom_datum = datum.sans_lifetime();

        let wkb_datum = OidFunctionCall1Coll(st_asbinary_func_oid, InvalidOid, geom_datum);

        let is_null = false;
        let wkb = Vec::<u8>::from_datum(wkb_datum, is_null).expect("cannot convert datum to wkb");
        Geometry(wkb)
    }
}