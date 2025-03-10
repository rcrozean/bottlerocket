use crate::error::{self, Result};
use crate::run::{KnownTestType, TestType};
use bottlerocket_types::agent_config::TufRepoConfig;
use bottlerocket_variant::Variant;
use handlebars::Handlebars;
use log::{debug, warn};
use maplit::btreemap;
use model::constants::{API_VERSION, NAMESPACE};
use model::test_manager::{SelectionParams, TestManager};
use model::Crd;
use pubsys_config::RepoConfig;
use serde::Deserialize;
use snafu::{OptionExt, ResultExt};
use std::collections::BTreeMap;
use testsys_config::{rendered_cluster_name, GenericVariantConfig, TestsysImages};

/// A type that is used for the creation of all CRDs.
pub struct CrdInput<'a> {
    pub client: &'a TestManager,
    pub arch: String,
    pub variant: Variant,
    pub config: GenericVariantConfig,
    pub repo_config: RepoConfig,
    pub starting_version: Option<String>,
    pub migrate_to_version: Option<String>,
    pub build_id: Option<String>,
    /// `CrdCreator::starting_image_id` function should be used instead of using this field, so
    /// it is not externally visible.
    pub(crate) starting_image_id: Option<String>,
    pub(crate) test_type: TestType,
    pub images: TestsysImages,
}

impl<'a> CrdInput<'a> {
    /// Retrieve the TUF repo information from `Infra.toml`
    pub fn tuf_repo_config(&self) -> Option<TufRepoConfig> {
        if let (Some(metadata_base_url), Some(targets_url)) = (
            &self.repo_config.metadata_base_url,
            &self.repo_config.targets_url,
        ) {
            debug!(
                "Using TUF metadata from Infra.toml, metadata: '{}', targets: '{}'",
                metadata_base_url, targets_url
            );
            Some(TufRepoConfig {
                metadata_url: format!("{}{}/{}/", metadata_base_url, &self.variant, &self.arch),
                targets_url: targets_url.to_string(),
            })
        } else {
            warn!("No TUF metadata was found in Infra.toml using the default TUF repos");
            None
        }
    }

    /// Create a set of labels for the CRD by adding `additional_labels` to the standard labels.
    pub fn labels(&self, additional_labels: BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut labels = btreemap! {
            "testsys/arch".to_string() => self.arch.to_string(),
            "testsys/variant".to_string() => self.variant.to_string(),
            "testsys/build-id".to_string() => self.build_id.to_owned().unwrap_or_default(),
            "testsys/test-type".to_string() => self.test_type.to_string(),
        };
        let mut add_labels = additional_labels;
        labels.append(&mut add_labels);
        labels
    }

    /// Determine all CRDs that have the same value for each `id_labels` as `labels`.
    pub async fn existing_crds(
        &self,
        labels: &BTreeMap<String, String>,
        id_labels: &[&str],
    ) -> Result<Vec<String>> {
        // Create a single string containing all `label=value` pairs.
        let checks = id_labels
            .iter()
            .map(|label| {
                labels
                    .get(&label.to_string())
                    .map(|value| format!("{}={}", label, value))
                    .context(error::InvalidSnafu {
                        what: format!("The label '{}' was missing", label),
                    })
            })
            .collect::<Result<Vec<String>>>()?
            .join(",");

        // Create a list of all CRD names that match all of the specified labels.
        Ok(self
            .client
            .list(&SelectionParams {
                labels: Some(checks),
                ..Default::default()
            })
            .await?
            .iter()
            .filter_map(Crd::name)
            .collect())
    }

    /// Use the provided userdata path to create the encoded userdata.
    pub fn encoded_userdata(&self) -> Result<Option<String>> {
        let userdata_path = match self.config.userdata.as_ref() {
            Some(path) => path,
            None => return Ok(None),
        };

        let userdata = std::fs::read_to_string(userdata_path).context(error::FileSnafu {
            path: userdata_path,
        })?;

        Ok(Some(base64::encode(userdata)))
    }

    /// Fill in the templated cluster name with `arch` and `variant`.
    fn rendered_cluster_name(&self, raw_cluster_name: String) -> Result<String> {
        Ok(rendered_cluster_name(
            raw_cluster_name,
            self.kube_arch(),
            self.kube_variant(),
        )?)
    }

    /// Get the k8s safe architecture name
    fn kube_arch(&self) -> String {
        self.arch.replace('_', "-")
    }

    /// Get the k8s safe variant name
    fn kube_variant(&self) -> String {
        self.variant.to_string().replace('.', "")
    }

    /// Bottlerocket cluster naming convention.
    fn default_cluster_name(&self) -> String {
        format!("{}-{}", self.kube_arch(), self.kube_variant())
    }

    /// Get a list of cluster_names for this variant. If there are no cluster names, the default
    /// cluster name will be used.
    fn cluster_names(&self) -> Result<Vec<String>> {
        Ok(if self.config.cluster_names.is_empty() {
            vec![self.default_cluster_name()]
        } else {
            self.config
                .cluster_names
                .iter()
                .map(String::to_string)
                // Fill the template fields in the clusters name before using it.
                .map(|cluster_name| self.rendered_cluster_name(cluster_name))
                .collect::<Result<Vec<String>>>()?
        })
    }

    /// Creates a `BTreeMap` of all configurable fields from this input
    fn config_fields(&self, cluster_name: &str) -> BTreeMap<String, String> {
        btreemap! {
            "arch".to_string() => self.arch.clone(),
            "variant".to_string() => self.variant.to_string(),
            "kube-arch".to_string() => self.kube_arch(),
            "kube-variant".to_string() => self.kube_variant(),
            "cluster-name".to_string() => cluster_name.to_string(),
            "instance-type".to_string() => some_or_null(&self.config.instance_type),
            "agent-role".to_string() => some_or_null(&self.config.agent_role),
            "conformance-image".to_string() => some_or_null(&self.config.conformance_image),
            "conformance-registry".to_string() => some_or_null(&self.config.conformance_registry),
        }
    }
}

/// Take the value of the `Option` or `"null"` if the `Option` was `None`
fn some_or_null(field: &Option<String>) -> String {
    field.to_owned().unwrap_or_else(|| "null".to_string())
}

/// The `CrdCreator` trait is used to create CRDs. Each variant family should have a `CrdCreator`
/// that is responsible for creating the CRDs needed for testing.
#[async_trait::async_trait]
pub(crate) trait CrdCreator: Sync {
    /// Return the image id that should be used for normal testing.
    fn image_id(&self, crd_input: &CrdInput) -> Result<String>;

    /// Return the image id that should be used as the starting point for migration testing.
    async fn starting_image_id(&self, crd_input: &CrdInput) -> Result<String>;

    /// Create a CRD for the cluster needed to launch Bottlerocket. If no cluster CRD is
    /// needed, `CreateCrdOutput::None` can be returned.
    async fn cluster_crd<'a>(&self, cluster_input: ClusterInput<'a>) -> Result<CreateCrdOutput>;

    /// Create a CRD to launch Bottlerocket. `CreateCrdOutput::None` can be returned if this CRD is
    /// not needed.
    async fn bottlerocket_crd<'a>(
        &self,
        bottlerocket_input: BottlerocketInput<'a>,
    ) -> Result<CreateCrdOutput>;

    /// Create a CRD that migrates Bottlerocket from one version to another.
    async fn migration_crd<'a>(
        &self,
        migration_input: MigrationInput<'a>,
    ) -> Result<CreateCrdOutput>;

    /// Create a testing CRD for this variant of Bottlerocket.
    async fn test_crd<'a>(&self, test_input: TestInput<'a>) -> Result<CreateCrdOutput>;

    /// Create a set of additional fields that may be used by an externally defined agent on top of
    /// the ones in `CrdInput`
    fn additional_fields(&self, _test_type: &str) -> BTreeMap<String, String> {
        Default::default()
    }

    /// Creates a set of CRDs for the specified variant and test type that can be added to a TestSys
    /// cluster.
    async fn create_crds(
        &self,
        test_type: &KnownTestType,
        crd_input: &CrdInput,
    ) -> Result<Vec<Crd>> {
        let mut crds = Vec::new();
        for cluster_name in &crd_input.cluster_names()? {
            let cluster_output = self
                .cluster_crd(ClusterInput {
                    cluster_name,
                    crd_input,
                })
                .await?;
            let cluster_crd_name = cluster_output.crd_name();
            if let Some(crd) = cluster_output.crd() {
                debug!("Cluster crd was created for '{}'", cluster_name);
                crds.push(crd)
            }
            match &test_type {
                KnownTestType::Conformance | KnownTestType::Quick => {
                    let bottlerocket_output = self
                        .bottlerocket_crd(BottlerocketInput {
                            cluster_crd_name: &cluster_crd_name,
                            image_id: self.image_id(crd_input)?,
                            test_type,
                            crd_input,
                        })
                        .await?;
                    let bottlerocket_crd_name = bottlerocket_output.crd_name();
                    if let Some(crd) = bottlerocket_output.crd() {
                        debug!("Bottlerocket crd was created for '{}'", cluster_name);
                        crds.push(crd)
                    }
                    let test_output = self
                        .test_crd(TestInput {
                            cluster_crd_name: &cluster_crd_name,
                            bottlerocket_crd_name: &bottlerocket_crd_name,
                            test_type,
                            crd_input,
                            prev_tests: Default::default(),
                            name_suffix: None,
                        })
                        .await?;
                    if let Some(crd) = test_output.crd() {
                        crds.push(crd)
                    }
                }
                KnownTestType::Migration => {
                    let image_id = if let Some(image_id) = &crd_input.starting_image_id {
                        debug!(
                            "Using the provided starting image id for migration testing '{}'",
                            image_id
                        );
                        image_id.to_string()
                    } else {
                        let image_id = self.starting_image_id(crd_input).await?;
                        debug!(
                            "A starting image id was not provided, '{}' will be used instead.",
                            image_id
                        );
                        image_id
                    };
                    let bottlerocket_output = self
                        .bottlerocket_crd(BottlerocketInput {
                            cluster_crd_name: &cluster_crd_name,
                            image_id,
                            test_type,
                            crd_input,
                        })
                        .await?;
                    let bottlerocket_crd_name = bottlerocket_output.crd_name();
                    if let Some(crd) = bottlerocket_output.crd() {
                        debug!("Bottlerocket crd was created for '{}'", cluster_name);
                        crds.push(crd)
                    }
                    let mut tests = Vec::new();
                    let test_output = self
                        .test_crd(TestInput {
                            cluster_crd_name: &cluster_crd_name,
                            bottlerocket_crd_name: &bottlerocket_crd_name,
                            test_type,
                            crd_input,
                            prev_tests: tests.clone(),
                            name_suffix: "-1-initial".into(),
                        })
                        .await?;
                    if let Some(name) = test_output.crd_name() {
                        tests.push(name)
                    }
                    if let Some(crd) = test_output.crd() {
                        crds.push(crd)
                    }
                    let migration_output = self
                        .migration_crd(MigrationInput {
                            cluster_crd_name: &cluster_crd_name,
                            bottlerocket_crd_name: &bottlerocket_crd_name,
                            crd_input,
                            prev_tests: tests.clone(),
                            name_suffix: "-2-migrate".into(),
                            migration_direction: MigrationDirection::Upgrade,
                        })
                        .await?;
                    if let Some(name) = migration_output.crd_name() {
                        tests.push(name)
                    }
                    if let Some(crd) = migration_output.crd() {
                        crds.push(crd)
                    }
                    let test_output = self
                        .test_crd(TestInput {
                            cluster_crd_name: &cluster_crd_name,
                            bottlerocket_crd_name: &bottlerocket_crd_name,
                            test_type,
                            crd_input,
                            prev_tests: tests.clone(),
                            name_suffix: "-3-migrated".into(),
                        })
                        .await?;
                    if let Some(name) = test_output.crd_name() {
                        tests.push(name)
                    }
                    if let Some(crd) = test_output.crd() {
                        crds.push(crd)
                    }
                    let migration_output = self
                        .migration_crd(MigrationInput {
                            cluster_crd_name: &cluster_crd_name,
                            bottlerocket_crd_name: &bottlerocket_crd_name,
                            crd_input,
                            prev_tests: tests.clone(),
                            name_suffix: "-4-migrate".into(),
                            migration_direction: MigrationDirection::Downgrade,
                        })
                        .await?;
                    if let Some(name) = migration_output.crd_name() {
                        tests.push(name)
                    }
                    if let Some(crd) = migration_output.crd() {
                        crds.push(crd)
                    }
                    let test_output = self
                        .test_crd(TestInput {
                            cluster_crd_name: &cluster_crd_name,
                            bottlerocket_crd_name: &bottlerocket_crd_name,
                            test_type,
                            crd_input,
                            prev_tests: tests,
                            name_suffix: "-5-final".into(),
                        })
                        .await?;
                    if let Some(crd) = test_output.crd() {
                        crds.push(crd)
                    }
                }
            }
        }

        Ok(crds)
    }

    /// Creates a set of CRDs for the specified variant and test type that can be added to a TestSys
    /// cluster.
    async fn create_custom_crds(
        &self,
        test_type: &str,
        crd_input: &CrdInput,
        crd_template_file_path: &str,
    ) -> Result<Vec<Crd>> {
        let mut crds = Vec::new();
        for cluster_name in &crd_input.cluster_names()? {
            let mut fields = crd_input.config_fields(cluster_name);
            fields.insert("api-version".to_string(), API_VERSION.to_string());
            fields.insert("namespace".to_string(), NAMESPACE.to_string());
            fields.insert("image-id".to_string(), self.image_id(crd_input)?);
            fields.append(&mut self.additional_fields(test_type));

            let mut handlebars = Handlebars::new();
            handlebars.set_strict_mode(true);
            let rendered_manifest = handlebars.render_template(
                &std::fs::read_to_string(crd_template_file_path).context(error::FileSnafu {
                    path: crd_template_file_path,
                })?,
                &fields,
            )?;

            for crd_doc in serde_yaml::Deserializer::from_str(&rendered_manifest) {
                let value =
                    serde_yaml::Value::deserialize(crd_doc).context(error::SerdeYamlSnafu {
                        what: "Unable to deserialize rendered manifest",
                    })?;
                let mut crd: Crd =
                    serde_yaml::from_value(value).context(error::SerdeYamlSnafu {
                        what: "The manifest did not match a `CRD`",
                    })?;
                // Add in the secrets from the config manually.
                match &mut crd {
                    Crd::Test(test) => {
                        test.spec.agent.secrets = Some(crd_input.config.secrets.clone())
                    }
                    Crd::Resource(resource) => {
                        resource.spec.agent.secrets = Some(crd_input.config.secrets.clone())
                    }
                }
                crds.push(crd);
            }
        }
        Ok(crds)
    }
}

/// The input used for cluster crd creation
pub struct ClusterInput<'a> {
    pub cluster_name: &'a String,
    pub crd_input: &'a CrdInput<'a>,
}

/// The input used for bottlerocket crd creation
pub struct BottlerocketInput<'a> {
    pub cluster_crd_name: &'a Option<String>,
    /// The image id that should be used by this CRD
    pub image_id: String,
    pub test_type: &'a KnownTestType,
    pub crd_input: &'a CrdInput<'a>,
}

/// The input used for test crd creation
pub struct TestInput<'a> {
    pub cluster_crd_name: &'a Option<String>,
    pub bottlerocket_crd_name: &'a Option<String>,
    pub test_type: &'a KnownTestType,
    pub crd_input: &'a CrdInput<'a>,
    /// The set of tests that have already been created that are related to this test
    pub prev_tests: Vec<String>,
    /// The suffix that should be appended to the end of the test name to prevent naming conflicts
    pub name_suffix: Option<&'a str>,
}

/// The input used for migration crd creation
pub struct MigrationInput<'a> {
    pub cluster_crd_name: &'a Option<String>,
    pub bottlerocket_crd_name: &'a Option<String>,
    pub crd_input: &'a CrdInput<'a>,
    /// The set of tests that have already been created that are related to this test
    pub prev_tests: Vec<String>,
    /// The suffix that should be appended to the end of the test name to prevent naming conflicts
    pub name_suffix: Option<&'a str>,
    pub migration_direction: MigrationDirection,
}

pub enum MigrationDirection {
    Upgrade,
    Downgrade,
}

pub enum CreateCrdOutput {
    /// A new CRD was created and needs to be applied to the cluster.
    NewCrd(Box<Crd>),
    /// An existing CRD is already representing this object.
    ExistingCrd(String),
    /// There is no CRD to create for this step of this family.
    None,
}

impl Default for CreateCrdOutput {
    fn default() -> Self {
        Self::None
    }
}

impl CreateCrdOutput {
    /// Get the name of the CRD that was created or already existed
    pub(crate) fn crd_name(&self) -> Option<String> {
        match self {
            CreateCrdOutput::NewCrd(crd) => {
                Some(crd.name().expect("A CRD is missing the name field."))
            }
            CreateCrdOutput::ExistingCrd(name) => Some(name.to_string()),
            CreateCrdOutput::None => None,
        }
    }

    /// Get the CRD if it was created
    pub(crate) fn crd(self) -> Option<Crd> {
        match self {
            CreateCrdOutput::NewCrd(crd) => Some(*crd),
            CreateCrdOutput::ExistingCrd(_) => None,
            CreateCrdOutput::None => None,
        }
    }
}
