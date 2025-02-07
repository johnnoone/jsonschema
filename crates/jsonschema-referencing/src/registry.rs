use std::{
    collections::{hash_map::Entry, HashSet, VecDeque},
    hash::{Hash, Hasher},
    pin::Pin,
    sync::Arc,
};

use ahash::{AHashMap, AHashSet, AHasher};
use fluent_uri::Uri;
use once_cell::sync::Lazy;
use serde_json::Value;

use crate::{
    anchors::{AnchorKey, AnchorKeyRef},
    cache::{SharedUriCache, UriCache},
    hasher::BuildNoHashHasher,
    list::List,
    meta,
    resource::{unescape_segment, InnerResourcePtr, JsonSchemaResource},
    uri,
    vocabularies::{self, VocabularySet},
    Anchor, DefaultRetriever, Draft, Error, Resolver, Resource, Retrieve,
};

// SAFETY: `Pin` guarantees stable memory locations for resource pointers,
// while `Arc` enables cheap sharing between multiple registries
type DocumentStore = AHashMap<Arc<Uri<String>>, Pin<Arc<Value>>>;
type ResourceMap = AHashMap<Arc<Uri<String>>, InnerResourcePtr>;

/// Pre-loaded registry containing all JSON Schema meta-schemas and their vocabularies
pub static SPECIFICATIONS: Lazy<Registry> = Lazy::new(|| {
    let pairs = meta::META_SCHEMAS.into_iter().map(|(uri, schema)| {
        (
            uri,
            Resource::from_contents(schema.clone()).expect("Invalid resource"),
        )
    });

    // The capacity is known upfront
    let mut documents = DocumentStore::with_capacity(18);
    let mut resources = ResourceMap::with_capacity(18);
    let mut anchors = AHashMap::with_capacity(8);
    let mut resolution_cache = UriCache::with_capacity(35);
    process_meta_schemas(
        pairs,
        &mut documents,
        &mut resources,
        &mut anchors,
        &mut resolution_cache,
    )
    .expect("Failed to process meta schemas");
    Registry {
        documents,
        resources,
        anchors,
        resolution_cache: resolution_cache.into_shared(),
    }
});

/// A registry of JSON Schema resources, each identified by their canonical URIs.
///
/// Registries store a collection of in-memory resources and their anchors.
/// They eagerly process all added resources, including their subresources and anchors.
/// This means that subresources contained within any added resources are immediately
/// discoverable and retrievable via their own IDs.
#[derive(Debug)]
pub struct Registry {
    // Pinned storage for primary documents
    documents: DocumentStore,
    pub(crate) resources: ResourceMap,
    anchors: AHashMap<AnchorKey, Anchor>,
    resolution_cache: SharedUriCache,
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        Self {
            documents: self.documents.clone(),
            resources: self.resources.clone(),
            anchors: self.anchors.clone(),
            resolution_cache: self.resolution_cache.clone(),
        }
    }
}

/// Configuration options for creating a [`Registry`].
pub struct RegistryOptions {
    retriever: Arc<dyn Retrieve>,
    draft: Draft,
}

impl RegistryOptions {
    /// Create a new [`RegistryOptions`] with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            retriever: Arc::new(DefaultRetriever),
            draft: Draft::default(),
        }
    }
    /// Set a custom retriever for the [`Registry`].
    #[must_use]
    pub fn retriever(mut self, retriever: Arc<dyn Retrieve>) -> Self {
        self.retriever = retriever;
        self
    }
    /// Set specification version under which the resources should be interpreted under.
    #[must_use]
    pub fn draft(mut self, draft: Draft) -> Self {
        self.draft = draft;
        self
    }
    /// Create a [`Registry`] with a single resource using these options.
    ///
    /// # Errors
    ///
    /// Returns an error if the URI is invalid or if there's an issue processing the resource.
    pub fn try_new(self, uri: impl AsRef<str>, resource: Resource) -> Result<Registry, Error> {
        Registry::try_new_impl(uri, resource, &*self.retriever, self.draft)
    }
    /// Create a [`Registry`] from multiple resources using these options.
    ///
    /// # Errors
    ///
    /// Returns an error if any URI is invalid or if there's an issue processing the resources.
    pub fn try_from_resources(
        self,
        pairs: impl Iterator<Item = (impl AsRef<str>, Resource)>,
    ) -> Result<Registry, Error> {
        Registry::try_from_resources_impl(pairs, &*self.retriever, self.draft)
    }
}

impl Default for RegistryOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// Get [`RegistryOptions`] for configuring a new [`Registry`].
    #[must_use]
    pub fn options() -> RegistryOptions {
        RegistryOptions::new()
    }
    /// Create a new [`Registry`] with a single resource.
    ///
    /// # Arguments
    ///
    /// * `uri` - The URI of the resource.
    /// * `resource` - The resource to add.
    ///
    /// # Errors
    ///
    /// Returns an error if the URI is invalid or if there's an issue processing the resource.
    pub fn try_new(uri: impl AsRef<str>, resource: Resource) -> Result<Self, Error> {
        Self::try_new_impl(uri, resource, &DefaultRetriever, Draft::default())
    }
    /// Create a new [`Registry`] from an iterator of (URI, Resource) pairs.
    ///
    /// # Arguments
    ///
    /// * `pairs` - An iterator of (URI, Resource) pairs.
    ///
    /// # Errors
    ///
    /// Returns an error if any URI is invalid or if there's an issue processing the resources.
    pub fn try_from_resources(
        pairs: impl Iterator<Item = (impl AsRef<str>, Resource)>,
    ) -> Result<Self, Error> {
        Self::try_from_resources_impl(pairs, &DefaultRetriever, Draft::default())
    }
    fn try_new_impl(
        uri: impl AsRef<str>,
        resource: Resource,
        retriever: &dyn Retrieve,
        draft: Draft,
    ) -> Result<Self, Error> {
        Self::try_from_resources_impl([(uri, resource)].into_iter(), retriever, draft)
    }
    fn try_from_resources_impl(
        pairs: impl Iterator<Item = (impl AsRef<str>, Resource)>,
        retriever: &dyn Retrieve,
        draft: Draft,
    ) -> Result<Self, Error> {
        let mut documents = AHashMap::new();
        let mut resources = ResourceMap::new();
        let mut anchors = AHashMap::new();
        let mut resolution_cache = UriCache::new();
        process_resources(
            pairs,
            retriever,
            &mut documents,
            &mut resources,
            &mut anchors,
            &mut resolution_cache,
            draft,
        )?;
        Ok(Registry {
            documents,
            resources,
            anchors,
            resolution_cache: resolution_cache.into_shared(),
        })
    }
    /// Create a new registry with a new resource.
    ///
    /// # Errors
    ///
    /// Returns an error if the URI is invalid or if there's an issue processing the resource.
    pub fn try_with_resource(
        self,
        uri: impl AsRef<str>,
        resource: Resource,
    ) -> Result<Registry, Error> {
        let draft = resource.draft();
        self.try_with_resources([(uri, resource)].into_iter(), draft)
    }
    /// Create a new registry with a new resource and using the given retriever.
    ///
    /// # Errors
    ///
    /// Returns an error if the URI is invalid or if there's an issue processing the resource.
    pub fn try_with_resource_and_retriever(
        self,
        uri: impl AsRef<str>,
        resource: Resource,
        retriever: &dyn Retrieve,
    ) -> Result<Registry, Error> {
        let draft = resource.draft();
        self.try_with_resources_and_retriever([(uri, resource)].into_iter(), retriever, draft)
    }
    /// Create a new registry with new resources.
    ///
    /// # Errors
    ///
    /// Returns an error if any URI is invalid or if there's an issue processing the resources.
    pub fn try_with_resources(
        self,
        pairs: impl Iterator<Item = (impl AsRef<str>, Resource)>,
        draft: Draft,
    ) -> Result<Registry, Error> {
        self.try_with_resources_and_retriever(pairs, &DefaultRetriever, draft)
    }
    /// Create a new registry with new resources and using the given retriever.
    ///
    /// # Errors
    ///
    /// Returns an error if any URI is invalid or if there's an issue processing the resources.
    pub fn try_with_resources_and_retriever(
        self,
        pairs: impl Iterator<Item = (impl AsRef<str>, Resource)>,
        retriever: &dyn Retrieve,
        draft: Draft,
    ) -> Result<Registry, Error> {
        let mut documents = self.documents;
        let mut resources = self.resources;
        let mut anchors = self.anchors;
        let mut resolution_cache = self.resolution_cache.into_local();
        process_resources(
            pairs,
            retriever,
            &mut documents,
            &mut resources,
            &mut anchors,
            &mut resolution_cache,
            draft,
        )?;
        Ok(Registry {
            documents,
            resources,
            anchors,
            resolution_cache: resolution_cache.into_shared(),
        })
    }
    /// Create a new [`Resolver`] for this registry with the given base URI.
    ///
    /// # Errors
    ///
    /// Returns an error if the base URI is invalid.
    pub fn try_resolver(&self, base_uri: &str) -> Result<Resolver, Error> {
        let base = uri::from_str(base_uri)?;
        Ok(self.resolver(base))
    }
    /// Create a new [`Resolver`] for this registry with a known valid base URI.
    #[must_use]
    pub fn resolver(&self, base_uri: Uri<String>) -> Resolver {
        Resolver::new(self, Arc::new(base_uri))
    }
    #[must_use]
    pub fn resolver_from_raw_parts(
        &self,
        base_uri: Arc<Uri<String>>,
        scopes: List<Uri<String>>,
    ) -> Resolver {
        Resolver::from_parts(self, base_uri, scopes)
    }
    pub(crate) fn anchor<'a>(&self, uri: &'a Uri<String>, name: &'a str) -> Result<&Anchor, Error> {
        let key = AnchorKeyRef::new(uri, name);
        if let Some(value) = self.anchors.get(key.borrow_dyn()) {
            return Ok(value);
        }
        let resource = &self.resources[uri];
        if let Some(id) = resource.id() {
            let uri = uri::from_str(id)?;
            let key = AnchorKeyRef::new(&uri, name);
            if let Some(value) = self.anchors.get(key.borrow_dyn()) {
                return Ok(value);
            }
        }
        if name.contains('/') {
            Err(Error::invalid_anchor(name.to_string()))
        } else {
            Err(Error::no_such_anchor(name.to_string()))
        }
    }
    /// Resolves a reference URI against a base URI using registry's cache.
    ///
    /// # Errors
    ///
    /// Returns an error if base has not schema or there is a fragment.
    pub fn resolve_against(&self, base: &Uri<&str>, uri: &str) -> Result<Arc<Uri<String>>, Error> {
        self.resolution_cache.resolve_against(base, uri)
    }
    /// Returns vocabulary set configured for given draft and contents.
    #[must_use]
    pub fn find_vocabularies(&self, draft: Draft, contents: &Value) -> VocabularySet {
        match draft.detect(contents) {
            Ok(draft) => draft.default_vocabularies(),
            Err(Error::UnknownSpecification { specification }) => {
                // Try to lookup the specification and find enabled vocabularies
                if let Ok(Some(resource)) =
                    uri::from_str(&specification).map(|uri| self.resources.get(&uri))
                {
                    if let Ok(Some(vocabularies)) = vocabularies::find(resource.contents()) {
                        return vocabularies;
                    }
                }
                draft.default_vocabularies()
            }
            _ => unreachable!(),
        }
    }
}

fn process_meta_schemas(
    pairs: impl Iterator<Item = (impl AsRef<str>, Resource)>,
    documents: &mut DocumentStore,
    resources: &mut ResourceMap,
    anchors: &mut AHashMap<AnchorKey, Anchor>,
    resolution_cache: &mut UriCache,
) -> Result<(), Error> {
    let mut queue = VecDeque::with_capacity(32);

    for (uri, resource) in pairs {
        let uri = uri::from_str(uri.as_ref().trim_end_matches('#'))?;
        let key = Arc::new(uri);
        let (draft, contents) = resource.into_inner();
        let boxed = Arc::pin(contents);
        let contents = std::ptr::addr_of!(*boxed);
        let resource = InnerResourcePtr::new(contents, draft);
        documents.insert(Arc::clone(&key), boxed);
        resources.insert(Arc::clone(&key), resource.clone());
        queue.push_back((key, resource));
    }

    // Process current queue and collect references to external resources
    while let Some((mut base, resource)) = queue.pop_front() {
        if let Some(id) = resource.id() {
            base = resolution_cache.resolve_against(&base.borrow(), id)?;
            resources.insert(base.clone(), resource.clone());
        }

        // Look for anchors
        for anchor in resource.anchors() {
            anchors.insert(AnchorKey::new(base.clone(), anchor.name()), anchor);
        }

        // Process subresources
        for contents in resource.draft().subresources_of(resource.contents()) {
            let subresource = InnerResourcePtr::new(contents, resource.draft());
            queue.push_back((base.clone(), subresource));
        }
    }
    Ok(())
}

fn process_resources(
    pairs: impl Iterator<Item = (impl AsRef<str>, Resource)>,
    retriever: &dyn Retrieve,
    documents: &mut DocumentStore,
    resources: &mut ResourceMap,
    anchors: &mut AHashMap<AnchorKey, Anchor>,
    resolution_cache: &mut UriCache,
    default_draft: Draft,
) -> Result<(), Error> {
    let mut queue = VecDeque::with_capacity(32);
    let mut seen = HashSet::with_hasher(BuildNoHashHasher::default());
    let mut external = AHashSet::new();
    let mut scratch = String::new();
    let mut refers_metaschemas = false;

    // SAFETY: Deduplicate input URIs keeping the last occurrence to prevent creation
    // of resources pointing to values that could be dropped by later insertions
    //let mut input_pairs: Vec<(Uri<String>, Resource)> = pairs
    //    .map(|(uri, resource)| Ok((uri::from_str(uri.as_ref().trim_end_matches('#'))?, resource)))
    //    .collect::<Result<Vec<_>, Error>>()?
    //    .into_iter()
    //    .rev()
    //    .collect();
    //input_pairs.dedup_by(|(lhs, _), (rhs, _)| lhs == rhs);

    for (uri, resource) in pairs {
        let uri = uri::from_str(uri.as_ref().trim_end_matches('#'))?;
        let key = Arc::new(uri);
        match documents.entry(Arc::clone(&key)) {
            Entry::Occupied(_) => {
                // SAFETY: Do not remove any existing documents so that all pointers are valid
                // The registry does not allow overriding existing resources right now
            }
            Entry::Vacant(entry) => {
                let (draft, contents) = resource.into_inner();
                let boxed = Arc::pin(contents);
                let contents = std::ptr::addr_of!(*boxed);
                let resource = InnerResourcePtr::new(contents, draft);
                resources.insert(Arc::clone(&key), resource.clone());
                queue.push_back((key, resource));
                entry.insert(boxed);
            }
        }
    }

    loop {
        if queue.is_empty() && external.is_empty() {
            break;
        }

        // Process current queue and collect references to external resources
        while let Some((mut base, resource)) = queue.pop_front() {
            if let Some(id) = resource.id() {
                base = resolution_cache.resolve_against(&base.borrow(), id)?;
                resources.insert(base.clone(), resource.clone());
            }

            // Look for anchors
            for anchor in resource.anchors() {
                anchors.insert(AnchorKey::new(base.clone(), anchor.name()), anchor);
            }

            // Collect references to external resources in this resource
            collect_external_resources(
                &base,
                resource.contents(),
                &mut external,
                &mut seen,
                resolution_cache,
                &mut scratch,
                &mut refers_metaschemas,
            )?;

            // Process subresources
            for contents in resource.draft().subresources_of(resource.contents()) {
                let subresource = InnerResourcePtr::new(contents, resource.draft());
                queue.push_back((base.clone(), subresource));
            }
        }
        // Retrieve external resources
        for uri in external.drain() {
            let mut fragmentless = uri.clone();
            fragmentless.set_fragment(None);
            if !resources.contains_key(&fragmentless) {
                let retrieved = retriever
                    .retrieve(&fragmentless.borrow())
                    .map_err(|err| Error::unretrievable(fragmentless.as_str(), err))?;

                let draft = default_draft.detect(&retrieved)?;
                let boxed = Arc::pin(retrieved);
                let contents = std::ptr::addr_of!(*boxed);
                let resource = InnerResourcePtr::new(contents, draft);
                let key = Arc::new(fragmentless);
                documents.insert(Arc::clone(&key), boxed);
                resources.insert(Arc::clone(&key), resource.clone());

                if let Some(fragment) = uri.fragment() {
                    // The original `$ref` could have a fragment that points to a place that won't
                    // be discovered via the regular sub-resources discovery. Therefore we need to
                    // explicitly check it
                    if let Some(resolved) = pointer(resource.contents(), fragment.as_str()) {
                        let draft = default_draft.detect(resolved)?;
                        let contents = std::ptr::addr_of!(*resolved);
                        let resource = InnerResourcePtr::new(contents, draft);
                        queue.push_back((Arc::clone(&key), resource));
                    }
                }

                queue.push_back((key, resource));
            }
        }
    }

    if refers_metaschemas {
        resources.reserve(SPECIFICATIONS.resources.len());
        for (key, resource) in &SPECIFICATIONS.resources {
            resources.insert(Arc::clone(key), resource.clone());
        }
        anchors.reserve(SPECIFICATIONS.anchors.len());
        for (key, anchor) in &SPECIFICATIONS.anchors {
            anchors.insert(key.clone(), anchor.clone());
        }
    }

    Ok(())
}

fn collect_external_resources(
    base: &Uri<String>,
    contents: &Value,
    collected: &mut AHashSet<Uri<String>>,
    seen: &mut HashSet<u64, BuildNoHashHasher>,
    resolution_cache: &mut UriCache,
    scratch: &mut String,
    refers_metaschemas: &mut bool,
) -> Result<(), Error> {
    // URN schemes are not supported for external resolution
    if base.scheme().as_str() == "urn" {
        return Ok(());
    }

    macro_rules! on_reference {
        ($reference:expr, $key:literal) => {
            // Skip well-known schema references
            if $reference.starts_with("https://json-schema.org/draft/")
                || $reference.starts_with("http://json-schema.org/draft-")
                || base.as_str().starts_with("https://json-schema.org/draft/")
            {
                if $key == "$ref" {
                    *refers_metaschemas = true;
                }
            } else if $reference != "#" {
                let mut hasher = AHasher::default();
                (base.as_str(), $reference).hash(&mut hasher);
                let hash = hasher.finish();
                if seen.insert(hash) {
                    // Handle local references separately as they may have nested references to external resources
                    if $reference.starts_with('#') {
                        if let Some(referenced) =
                            pointer(contents, $reference.trim_start_matches('#'))
                        {
                            collect_external_resources(
                                base,
                                referenced,
                                collected,
                                seen,
                                resolution_cache,
                                scratch,
                                refers_metaschemas,
                            )?;
                        }
                    } else {
                        let resolved = if base.has_fragment() {
                            let mut base_without_fragment = base.clone();
                            base_without_fragment.set_fragment(None);

                            let (path, fragment) = match $reference.split_once('#') {
                                Some((path, fragment)) => (path, Some(fragment)),
                                None => ($reference, None),
                            };

                            let mut resolved = (*resolution_cache
                                .resolve_against(&base_without_fragment.borrow(), path)?)
                            .clone();
                            // Add the fragment back if present
                            if let Some(fragment) = fragment {
                                // It is cheaper to check if it is properly encoded than allocate given that
                                // the majority of inputs do not need to be additionally encoded
                                if let Some(encoded) = uri::EncodedString::new(fragment) {
                                    resolved = resolved.with_fragment(Some(encoded));
                                } else {
                                    uri::encode_to(fragment, scratch);
                                    resolved = resolved.with_fragment(Some(
                                        uri::EncodedString::new_or_panic(scratch),
                                    ));
                                    scratch.clear();
                                }
                            }
                            resolved
                        } else {
                            (*resolution_cache.resolve_against(&base.borrow(), $reference)?).clone()
                        };

                        collected.insert(resolved);
                    }
                }
            }
        };
    }

    if let Some(object) = contents.as_object() {
        if object.len() < 3 {
            for (key, value) in object {
                if key == "$ref" {
                    if let Some(reference) = value.as_str() {
                        on_reference!(reference, "$ref");
                    }
                } else if key == "$schema" {
                    if let Some(reference) = value.as_str() {
                        on_reference!(reference, "$schema");
                    }
                }
            }
        } else {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                on_reference!(reference, "$ref");
            }
            if let Some(reference) = object.get("$schema").and_then(Value::as_str) {
                on_reference!(reference, "$schema");
            }
        }
    }
    Ok(())
}

// A slightly faster version of pointer resolution based on `Value::pointer` from `serde_json`.
fn pointer<'a>(document: &'a Value, pointer: &str) -> Option<&'a Value> {
    if pointer.is_empty() {
        return Some(document);
    }
    if !pointer.starts_with('/') {
        return None;
    }
    pointer.split('/').skip(1).map(unescape_segment).try_fold(
        document,
        |target, token| match target {
            Value::Object(map) => map.get(&*token),
            Value::Array(list) => parse_index(&token).and_then(|x| list.get(x)),
            _ => None,
        },
    )
}

// Taken from `serde_json`.
fn parse_index(s: &str) -> Option<usize> {
    if s.starts_with('+') || (s.starts_with('0') && s.len() != 1) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, sync::Arc};

    use ahash::AHashMap;
    use fluent_uri::Uri;
    use serde_json::{json, Value};
    use test_case::test_case;

    use crate::{uri::from_str, Draft, Registry, Resource, Retrieve};

    use super::{RegistryOptions, SPECIFICATIONS};

    #[test]
    fn test_invalid_uri_on_registry_creation() {
        let schema = Draft::Draft202012.create_resource(json!({}));
        let result = Registry::try_new(":/example.com", schema);
        let error = result.expect_err("Should fail");

        assert_eq!(
            error.to_string(),
            "Invalid URI reference ':/example.com': unexpected character at index 0"
        );
        let source_error = error.source().expect("Should have a source");
        let inner_source = source_error.source().expect("Should have a source");
        assert_eq!(inner_source.to_string(), "unexpected character at index 0");
    }

    #[test]
    fn test_lookup_unresolvable_url() {
        // Create a registry with a single resource
        let schema = Draft::Draft202012.create_resource(json!({
            "type": "object",
            "properties": {
                "foo": { "type": "string" }
            }
        }));
        let registry =
            Registry::try_new("http://example.com/schema1", schema).expect("Invalid resources");

        // Attempt to create a resolver for a URL not in the registry
        let resolver = registry
            .try_resolver("http://example.com/non_existent_schema")
            .expect("Invalid base URI");

        let result = resolver.lookup("");

        assert_eq!(
            result.unwrap_err().to_string(),
            "Resource 'http://example.com/non_existent_schema' is not present in a registry and retrieving it failed: Retrieving external resources is not supported once the registry is populated"
        );
    }

    struct TestRetriever {
        schemas: AHashMap<String, Value>,
    }

    impl TestRetriever {
        fn new(schemas: AHashMap<String, Value>) -> Self {
            TestRetriever { schemas }
        }
    }

    impl Retrieve for TestRetriever {
        fn retrieve(
            &self,
            uri: &Uri<&str>,
        ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
            if let Some(value) = self.schemas.get(uri.as_str()) {
                Ok(value.clone())
            } else {
                Err(format!("Failed to find {uri}").into())
            }
        }
    }

    fn create_test_retriever(schemas: &[(&str, Value)]) -> TestRetriever {
        TestRetriever::new(
            schemas
                .iter()
                .map(|&(k, ref v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    struct TestCase {
        input_resources: Vec<(&'static str, Value)>,
        remote_resources: Vec<(&'static str, Value)>,
        expected_resolved_uris: Vec<&'static str>,
    }

    #[test_case(
        TestCase {
            input_resources: vec![
                ("http://example.com/schema1", json!({"$ref": "http://example.com/schema2"})),
            ],
            remote_resources: vec![
                ("http://example.com/schema2", json!({"type": "object"})),
            ],
            expected_resolved_uris: vec!["http://example.com/schema1", "http://example.com/schema2"],
        }
    ;"External ref at top")]
    #[test_case(
        TestCase {
            input_resources: vec![
                ("http://example.com/schema1", json!({
                    "$defs": {
                        "subschema": {"type": "string"}
                    },
                    "$ref": "#/$defs/subschema"
                })),
            ],
            remote_resources: vec![],
            expected_resolved_uris: vec!["http://example.com/schema1"],
        }
    ;"Internal ref at top")]
    #[test_case(
        TestCase {
            input_resources: vec![
                ("http://example.com/schema1", json!({"$ref": "http://example.com/schema2"})),
                ("http://example.com/schema2", json!({"type": "object"})),
            ],
            remote_resources: vec![],
            expected_resolved_uris: vec!["http://example.com/schema1", "http://example.com/schema2"],
        }
    ;"Ref to later resource")]
    #[test_case(
    TestCase {
            input_resources: vec![
                ("http://example.com/schema1", json!({
                    "type": "object",
                    "properties": {
                        "prop1": {"$ref": "http://example.com/schema2"}
                    }
                })),
            ],
            remote_resources: vec![
                ("http://example.com/schema2", json!({"type": "string"})),
            ],
            expected_resolved_uris: vec!["http://example.com/schema1", "http://example.com/schema2"],
        }
    ;"External ref in subresource")]
    #[test_case(
        TestCase {
            input_resources: vec![
                ("http://example.com/schema1", json!({
                    "type": "object",
                    "properties": {
                        "prop1": {"$ref": "#/$defs/subschema"}
                    },
                    "$defs": {
                        "subschema": {"type": "string"}
                    }
                })),
            ],
            remote_resources: vec![],
            expected_resolved_uris: vec!["http://example.com/schema1"],
        }
    ;"Internal ref in subresource")]
    #[test_case(
        TestCase {
            input_resources: vec![
                ("file:///schemas/main.json", json!({"$ref": "file:///schemas/external.json"})),
            ],
            remote_resources: vec![
                ("file:///schemas/external.json", json!({"type": "object"})),
            ],
            expected_resolved_uris: vec!["file:///schemas/main.json", "file:///schemas/external.json"],
        }
    ;"File scheme: external ref at top")]
    #[test_case(
        TestCase {
            input_resources: vec![
                ("file:///schemas/main.json", json!({"$ref": "subfolder/schema.json"})),
            ],
            remote_resources: vec![
                ("file:///schemas/subfolder/schema.json", json!({"type": "string"})),
            ],
            expected_resolved_uris: vec!["file:///schemas/main.json", "file:///schemas/subfolder/schema.json"],
        }
    ;"File scheme: relative path ref")]
    #[test_case(
        TestCase {
            input_resources: vec![
                ("file:///schemas/main.json", json!({
                    "type": "object",
                    "properties": {
                        "local": {"$ref": "local.json"},
                        "remote": {"$ref": "http://example.com/schema"}
                    }
                })),
            ],
            remote_resources: vec![
                ("file:///schemas/local.json", json!({"type": "string"})),
                ("http://example.com/schema", json!({"type": "number"})),
            ],
            expected_resolved_uris: vec![
                "file:///schemas/main.json",
                "file:///schemas/local.json",
                "http://example.com/schema"
            ],
        }
    ;"File scheme: mixing with http scheme")]
    #[test_case(
        TestCase {
            input_resources: vec![
                ("file:///C:/schemas/main.json", json!({"$ref": "/D:/other_schemas/schema.json"})),
            ],
            remote_resources: vec![
                ("file:///D:/other_schemas/schema.json", json!({"type": "boolean"})),
            ],
            expected_resolved_uris: vec![
                "file:///C:/schemas/main.json",
                "file:///D:/other_schemas/schema.json"
            ],
        }
    ;"File scheme: absolute path in Windows style")]
    #[test_case(
        TestCase {
            input_resources: vec![
                ("http://example.com/schema1", json!({"$ref": "http://example.com/schema2"})),
            ],
            remote_resources: vec![
                ("http://example.com/schema2", json!({"$ref": "http://example.com/schema3"})),
                ("http://example.com/schema3", json!({"$ref": "http://example.com/schema4"})),
                ("http://example.com/schema4", json!({"$ref": "http://example.com/schema5"})),
                ("http://example.com/schema5", json!({"type": "object"})),
            ],
            expected_resolved_uris: vec![
                "http://example.com/schema1",
                "http://example.com/schema2",
                "http://example.com/schema3",
                "http://example.com/schema4",
                "http://example.com/schema5",
            ],
        }
    ;"Four levels of external references")]
    #[test_case(
        TestCase {
            input_resources: vec![
                ("http://example.com/schema1", json!({"$ref": "http://example.com/schema2"})),
            ],
            remote_resources: vec![
                ("http://example.com/schema2", json!({"$ref": "http://example.com/schema3"})),
                ("http://example.com/schema3", json!({"$ref": "http://example.com/schema4"})),
                ("http://example.com/schema4", json!({"$ref": "http://example.com/schema5"})),
                ("http://example.com/schema5", json!({"$ref": "http://example.com/schema6"})),
                ("http://example.com/schema6", json!({"$ref": "http://example.com/schema1"})),
            ],
            expected_resolved_uris: vec![
                "http://example.com/schema1",
                "http://example.com/schema2",
                "http://example.com/schema3",
                "http://example.com/schema4",
                "http://example.com/schema5",
                "http://example.com/schema6",
            ],
        }
    ;"Five levels of external references with circular reference")]
    fn test_references_processing(test_case: TestCase) {
        let retriever = create_test_retriever(&test_case.remote_resources);

        let input_pairs = test_case
            .input_resources
            .clone()
            .into_iter()
            .map(|(uri, value)| {
                (
                    uri,
                    Resource::from_contents(value).expect("Invalid resource"),
                )
            });

        let registry = Registry::options()
            .retriever(Arc::new(retriever))
            .try_from_resources(input_pairs)
            .expect("Invalid resources");
        // Verify that all expected URIs are resolved and present in resources
        for uri in test_case.expected_resolved_uris {
            let resolver = registry.try_resolver("").expect("Invalid base URI");
            assert!(resolver.lookup(uri).is_ok());
        }
    }

    #[test]
    fn test_default_retriever_with_remote_refs() {
        let result = Registry::try_from_resources(
            [(
                "http://example.com/schema1",
                Resource::from_contents(json!({"$ref": "http://example.com/schema2"}))
                    .expect("Invalid resource"),
            )]
            .into_iter(),
        );
        let error = result.expect_err("Should fail");
        assert_eq!(error.to_string(), "Resource 'http://example.com/schema2' is not present in a registry and retrieving it failed: Default retriever does not fetch resources");
        assert!(error.source().is_some());
    }

    #[test]
    fn test_options() {
        let _registry = RegistryOptions::default()
            .try_new("", Draft::default().create_resource(json!({})))
            .expect("Invalid resources");
    }

    #[test]
    fn test_registry_with_duplicate_input_uris() {
        let input_resources = vec![
            (
                "http://example.com/schema",
                json!({
                    "type": "object",
                    "properties": {
                        "foo": { "type": "string" }
                    }
                }),
            ),
            (
                "http://example.com/schema",
                json!({
                    "type": "object",
                    "properties": {
                        "bar": { "type": "number" }
                    }
                }),
            ),
        ];

        let result = Registry::try_from_resources(
            input_resources
                .into_iter()
                .map(|(uri, value)| (uri, Draft::Draft202012.create_resource(value))),
        );

        assert!(
            result.is_ok(),
            "Failed to create registry with duplicate input URIs"
        );
        let registry = result.unwrap();

        let resource = registry
            .resources
            .get(&from_str("http://example.com/schema").expect("Invalid URI"))
            .unwrap();
        let properties = resource
            .contents()
            .get("properties")
            .and_then(|v| v.as_object())
            .unwrap();

        assert!(
            !properties.contains_key("bar"),
            "Registry should contain the earliest added schema"
        );
        assert!(
            properties.contains_key("foo"),
            "Registry should contain the overwritten schema"
        );
    }

    #[test]
    fn test_resolver_debug() {
        let registry = SPECIFICATIONS
            .clone()
            .try_with_resource(
                "http://example.com",
                Resource::from_contents(json!({})).expect("Invalid resource"),
            )
            .expect("Invalid resource");
        let resolver = registry
            .try_resolver("http://127.0.0.1/schema")
            .expect("Invalid base URI");
        assert_eq!(
            format!("{resolver:?}"),
            "Resolver { base_uri: \"http://127.0.0.1/schema\", scopes: \"[]\" }"
        );
    }

    #[test]
    fn test_try_with_resource() {
        let registry = SPECIFICATIONS
            .clone()
            .try_with_resource(
                "http://example.com",
                Resource::from_contents(json!({})).expect("Invalid resource"),
            )
            .expect("Invalid resource");
        let resolver = registry.try_resolver("").expect("Invalid base URI");
        let resolved = resolver
            .lookup("http://json-schema.org/draft-06/schema#/definitions/schemaArray")
            .expect("Lookup failed");
        assert_eq!(
            resolved.contents(),
            &json!({
                "type": "array",
                "minItems": 1,
                "items": { "$ref": "#" }
            })
        );
    }

    #[test]
    fn test_try_with_resource_and_retriever() {
        let retriever =
            create_test_retriever(&[("http://example.com/schema2", json!({"type": "object"}))]);
        let registry = SPECIFICATIONS
            .clone()
            .try_with_resource_and_retriever(
                "http://example.com",
                Resource::from_contents(json!({"$ref": "http://example.com/schema2"}))
                    .expect("Invalid resource"),
                &retriever,
            )
            .expect("Invalid resource");
        let resolver = registry.try_resolver("").expect("Invalid base URI");
        let resolved = resolver
            .lookup("http://example.com/schema2")
            .expect("Lookup failed");
        assert_eq!(resolved.contents(), &json!({"type": "object"}));
    }

    #[test]
    fn test_invalid_reference() {
        // Found via fuzzing
        let resource = Draft::Draft202012.create_resource(json!({"$schema": "$##"}));
        let _ = Registry::try_new("http://#/", resource);
    }
}
