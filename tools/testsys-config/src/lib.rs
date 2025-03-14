use bottlerocket_variant::Variant;
pub use error::Error;
use handlebars::Handlebars;
use log::warn;
use maplit::btreemap;
use model::constants::TESTSYS_VERSION;
use model::SecretName;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
pub type Result<T> = std::result::Result<T, error::Error>;

/// Configuration needed to run tests
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct TestConfig {
    /// High level configuration for TestSys
    pub test: Option<Test>,

    #[serde(flatten, serialize_with = "toml::ser::tables_last")]
    /// Configuration for testing variants
    pub configs: HashMap<String, GenericConfig>,
}

impl TestConfig {
    /// Deserializes a TestConfig from a given path
    pub fn from_path<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let test_config_str = fs::read_to_string(path).context(error::FileSnafu { path })?;
        toml::from_str(&test_config_str).context(error::InvalidTomlSnafu { path })
    }

    /// Deserializes a TestConfig from a given path, if it exists, otherwise builds a default
    /// config
    pub fn from_path_or_default<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        if path.as_ref().exists() {
            Self::from_path(path)
        } else {
            warn!(
                "No test config was found at '{}'. Using the default config.",
                path.as_ref().display()
            );
            Ok(Self::default())
        }
    }

    /// Create a single config for the `variant` and `arch` from this test configuration by
    /// determining a list of tables that contain information relevant to the arch, variant
    /// combination. Then, the tables are reduced to a single config by selecting values from the
    /// table based on the order of precedence. If `starting_config` is provided it will be used as
    /// the config with the highest precedence.
    pub fn reduced_config<S>(
        &self,
        variant: &Variant,
        arch: S,
        starting_config: Option<GenericVariantConfig>,
        test_type: &str,
    ) -> GenericVariantConfig
    where
        S: Into<String>,
    {
        let arch = arch.into();
        // Starting with a list of keys ordered by precedence, return a single config with values
        // selected by the order of the list.
        config_keys(variant)
            // Convert the vec of keys in to an iterator of keys.
            .into_iter()
            // Convert the iterator of keys to and iterator of Configs. If the key does not have a
            // configuration in the config file, remove it from the iterator.
            .filter_map(|key| self.configs.get(&key).cloned())
            // Expand the `test_type` configuration
            .flat_map(|config| vec![config.test(test_type), config])
            // Take the iterator of configurations and extract the arch specific config and the
            // non-arch specific config for each config. Then, convert them into a single iterator.
            .flat_map(|config| vec![config.for_arch(&arch), config.config])
            // Take the iterator of configurations and merge them into a single config by populating
            // each field with the first value that is not `None` while following the list of
            // precedence.
            .fold(
                starting_config.unwrap_or_default(),
                GenericVariantConfig::merge,
            )
    }
}

/// High level configurations for a test
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Test {
    /// The name of the repo in `Infra.toml` that should be used for testing
    pub repo: Option<String>,

    /// The name of the vSphere data center in `Infra.toml` that should be used for testing
    /// If no data center is provided, the first one in `vmware.datacenters` will be used
    pub datacenter: Option<String>,

    #[serde(flatten)]
    /// The URI of TestSys images
    pub testsys_images: TestsysImages,

    /// A registry containing all TestSys images
    pub testsys_image_registry: Option<String>,

    /// The tag that should be used for TestSys images
    pub testsys_image_tag: Option<String>,
}

/// Create a vec of relevant keys for this variant ordered from most specific to least specific.
fn config_keys(variant: &Variant) -> Vec<String> {
    let (family_flavor, platform_flavor) = variant
        .variant_flavor()
        .map(|flavor| {
            (
                format!("{}-{}", variant.family(), flavor),
                format!("{}-{}", variant.platform(), flavor),
            )
        })
        .unwrap_or_default();

    // The keys used to describe configuration (most specific -> least specific)
    vec![
        variant.to_string(),
        family_flavor,
        variant.family().to_string(),
        platform_flavor,
        variant.platform().to_string(),
    ]
}

/// All configurations for a specific config level, i.e `<PLATFORM>-<FLAVOR>`
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct GenericConfig {
    #[serde(default)]
    aarch64: GenericVariantConfig,
    #[serde(default)]
    x86_64: GenericVariantConfig,
    #[serde(default, flatten)]
    config: GenericVariantConfig,
    #[serde(default)]
    configuration: HashMap<String, GenericConfig>,
}

impl GenericConfig {
    /// Get the configuration for a specific arch.
    pub fn for_arch<S>(&self, arch: S) -> GenericVariantConfig
    where
        S: Into<String>,
    {
        match arch.into().as_str() {
            "x86_64" => self.x86_64.clone(),
            "aarch64" => self.aarch64.clone(),
            _ => Default::default(),
        }
    }

    /// Get the configuration for a specific test type.
    pub fn test<S>(&self, test_type: S) -> GenericConfig
    where
        S: AsRef<str>,
    {
        self.configuration
            .get(test_type.as_ref())
            .cloned()
            .unwrap_or_default()
    }
}

/// The configuration for a specific config level (<PLATFORM>-<FLAVOR>). This may or may not be arch
/// specific depending on it's location in `GenericConfig`.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct GenericVariantConfig {
    /// The names of all clusters this variant should be tested over. This is particularly useful
    /// for testing Bottlerocket on ipv4 and ipv6 clusters.
    #[serde(default)]
    pub cluster_names: Vec<String>,
    /// The instance type that instances should be launched with
    pub instance_type: Option<String>,
    /// The secrets needed by the agents
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretName>,
    /// The role that should be assumed for this particular variant
    pub agent_role: Option<String>,
    /// The custom images used for conformance testing
    pub conformance_image: Option<String>,
    /// The custom registry used for conformance testing
    pub conformance_registry: Option<String>,
    /// The endpoint IP to reserve for the vSphere control plane VMs when creating a K8s cluster
    pub control_plane_endpoint: Option<String>,
    /// The path to userdata that should be used for Bottlerocket launch
    pub userdata: Option<String>,
}

impl GenericVariantConfig {
    /// Overwrite the unset values of `self` with the set values of `other`
    fn merge(self, other: Self) -> Self {
        let cluster_names = if self.cluster_names.is_empty() {
            other.cluster_names
        } else {
            self.cluster_names
        };

        let secrets = if self.secrets.is_empty() {
            other.secrets
        } else {
            self.secrets
        };

        Self {
            cluster_names,
            instance_type: self.instance_type.or(other.instance_type),
            secrets,
            agent_role: self.agent_role.or(other.agent_role),
            conformance_image: self.conformance_image.or(other.conformance_image),
            conformance_registry: self.conformance_registry.or(other.conformance_registry),
            control_plane_endpoint: self.control_plane_endpoint.or(other.control_plane_endpoint),
            userdata: self.userdata.or(other.userdata),
        }
    }
}

/// Fill in the templated cluster name with `arch` and `variant`.
pub fn rendered_cluster_name<S1, S2>(cluster_name: String, arch: S1, variant: S2) -> Result<String>
where
    S1: Into<String>,
    S2: Into<String>,
{
    let mut cluster_template = Handlebars::new();
    cluster_template.register_template_string("cluster_name", cluster_name)?;
    Ok(cluster_template.render(
        "cluster_name",
        &btreemap! {"arch".to_string() => arch.into(), "variant".to_string() => variant.into()},
    )?)
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct TestsysImages {
    pub eks_resource_agent_image: Option<String>,
    pub ecs_resource_agent_image: Option<String>,
    pub vsphere_k8s_cluster_resource_agent_image: Option<String>,
    pub ec2_resource_agent_image: Option<String>,
    pub vsphere_vm_resource_agent_image: Option<String>,
    pub sonobuoy_test_agent_image: Option<String>,
    pub ecs_test_agent_image: Option<String>,
    pub migration_test_agent_image: Option<String>,
    pub controller_image: Option<String>,
    pub testsys_agent_pull_secret: Option<String>,
}

impl TestsysImages {
    /// Create an images config for a specific registry.
    pub fn new<S>(registry: S, tag: Option<String>) -> Self
    where
        S: Into<String>,
    {
        let registry = registry.into();
        let tag = tag.unwrap_or_else(|| format!("v{}", TESTSYS_VERSION));
        Self {
            eks_resource_agent_image: Some(format!("{}/eks-resource-agent:{tag}", registry)),
            ecs_resource_agent_image: Some(format!("{}/ecs-resource-agent:{tag}", registry)),
            vsphere_k8s_cluster_resource_agent_image: Some(format!(
                "{}/vsphere-k8s-cluster-resource-agent:{tag}",
                registry
            )),
            ec2_resource_agent_image: Some(format!("{}/ec2-resource-agent:{tag}", registry)),
            vsphere_vm_resource_agent_image: Some(format!(
                "{}/vsphere-vm-resource-agent:{tag}",
                registry
            )),
            sonobuoy_test_agent_image: Some(format!("{}/sonobuoy-test-agent:{tag}", registry)),
            ecs_test_agent_image: Some(format!("{}/ecs-test-agent:{tag}", registry)),
            migration_test_agent_image: Some(format!("{}/migration-test-agent:{tag}", registry)),
            controller_image: Some(format!("{}/controller:{tag}", registry)),
            testsys_agent_pull_secret: None,
        }
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            eks_resource_agent_image: self
                .eks_resource_agent_image
                .or(other.eks_resource_agent_image),
            ecs_resource_agent_image: self
                .ecs_resource_agent_image
                .or(other.ecs_resource_agent_image),
            vsphere_k8s_cluster_resource_agent_image: self
                .vsphere_k8s_cluster_resource_agent_image
                .or(other.vsphere_k8s_cluster_resource_agent_image),
            vsphere_vm_resource_agent_image: self
                .vsphere_vm_resource_agent_image
                .or(other.vsphere_vm_resource_agent_image),
            ec2_resource_agent_image: self
                .ec2_resource_agent_image
                .or(other.ec2_resource_agent_image),
            sonobuoy_test_agent_image: self
                .sonobuoy_test_agent_image
                .or(other.sonobuoy_test_agent_image),
            ecs_test_agent_image: self.ecs_test_agent_image.or(other.ecs_test_agent_image),
            migration_test_agent_image: self
                .migration_test_agent_image
                .or(other.migration_test_agent_image),
            controller_image: self.controller_image.or(other.controller_image),
            testsys_agent_pull_secret: self
                .testsys_agent_pull_secret
                .or(other.testsys_agent_pull_secret),
        }
    }

    pub fn public_images() -> Self {
        Self::new("public.ecr.aws/bottlerocket-test-system", None)
    }
}

mod error {
    use snafu::Snafu;
    use std::io;
    use std::path::PathBuf;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(super)))]
    pub enum Error {
        #[snafu(display("Failed to read '{}': {}", path.display(), source))]
        File { path: PathBuf, source: io::Error },

        #[snafu(display("Invalid config file at '{}': {}", path.display(), source))]
        InvalidToml {
            path: PathBuf,
            source: toml::de::Error,
        },

        #[snafu(display("Invalid lock file at '{}': {}", path.display(), source))]
        InvalidLock {
            path: PathBuf,
            source: serde_yaml::Error,
        },

        #[snafu(display("Missing config: {}", what))]
        MissingConfig { what: String },

        #[snafu(display("Failed to get parent of path: {}", path.display()))]
        Parent { path: PathBuf },

        #[snafu(
            context(false),
            display("Failed to create template for cluster name: {}", source)
        )]
        TemplateError { source: handlebars::TemplateError },

        #[snafu(
            context(false),
            display("Failed to render templated cluster name: {}", source)
        )]
        RenderError { source: handlebars::RenderError },
    }
}
