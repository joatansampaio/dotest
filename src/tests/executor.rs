use std::collections::HashMap;
use crate::core::executor::{
    strip_params, is_test_attribute, extract_method_name, extract_class_name, enrich, parse_cs_content,
    build_discovery_entries, format_discovery_failure,
};

/// Parametric tests: many `dotnet test -t` lines map to one source method → one leaf, merged count.
#[test]
fn test_build_discovery_parametric_collapses_to_one_row() {
    let mut methods = HashMap::new();
    methods.insert(
        "ValueTypes".to_string(),
        vec![("Tests".to_string(), "Ns.Tests.JsonTests".to_string())],
    );
    let display_names = vec![
        "ValueTypes(\"1\")".to_string(),
        "ValueTypes(\"2\")".to_string(),
        "ValueTypes(\"3\")".to_string(),
    ];
    let class_map = HashMap::new();
    let out = build_discovery_entries(&display_names, &methods, &class_map);
    assert_eq!(out.len(), 1, "same method, multiple list lines -> one row with merged count");
    assert_eq!(out[0].2, 3);
}

/// UTF-8 BOM before `namespace` must not hide the namespace (Visual Studio default for new files).
#[test]
fn test_parse_cs_content_utf8_bom_before_namespace() {
    let content = "\u{feff}namespace Acme.Tests;\npublic class T {\n    [Test] public void M() {}\n}\n";
    let mut methods = HashMap::new();
    let mut classes = HashMap::new();
    parse_cs_content(content, "Acme", &mut methods, &mut classes);
    assert!(methods.contains_key("M"), "method should be found when BOM precedes namespace");
    let (_, qc) = &methods["M"][0];
    assert!(
        qc.starts_with("Acme.Tests."),
        "qualified class should include namespace, got {}",
        qc
    );
}

#[test]
fn test_discovery_failure_explains_missing_sdk_from_global_json() {
    let stderr = r#"The command could not be loaded, possibly because:
  * You intended to execute a .NET SDK command:
      A compatible .NET SDK was not found.

Requested SDK version: 7.0.101
global.json file: C:\Users\Joatan\Repos\wolverine\global.json

Installed SDKs:
10.0.100-rc.1.25451.107 [C:\Program Files\dotnet\sdk]"#;

    let message = format_discovery_failure(Some(1), "", stderr, true, true, None);

    assert!(message.contains("Test discovery failed while running `dotnet test /p:UseSharedCompilation=true /p:BaseOutputPath=bin/dotest/ -t --no-build --no-restore`"));
    assert!(message.contains("The .NET SDK selected by global.json is not installed or cannot be used"));
    assert!(message.contains("Install the requested SDK, or update global.json"));
    assert!(message.contains("Requested SDK version: 7.0.101"));
}

#[test]
fn test_discovery_parsing_extracts_tests_even_with_noise_before_list() {
    let stdout = r#"Some build warning
The following Tests are available:
    Ns.ClassA.TestOne
    Ns.ClassB.TestTwo(param: 5)
"#;
    let mut tests = Vec::new();
    let mut capturing = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "The following Tests are available:" {
            capturing = true;
            continue;
        }
        if capturing && !trimmed.is_empty() {
            tests.push(trimmed.to_string());
        }
    }

    assert_eq!(tests.len(), 2);
    assert_eq!(tests[0], "Ns.ClassA.TestOne");
    assert_eq!(tests[1], "Ns.ClassB.TestTwo(param: 5)");
}

/// Tree path is disk-folder first (like VS Code), VSTest filter stays fully qualified.
#[test]
fn test_tree_fqn_disk_folder_then_class_method() {
    let mut methods = HashMap::new();
    methods.insert(
        "DoThing".to_string(),
        vec![(
            "Groups".to_string(),
            "Tmly.Test.Groups.OrgTreeTests".to_string(),
        )],
    );
    let display_names = vec!["DoThing(\"a\")".to_string()];
    let class_map = HashMap::new();
    let out = build_discovery_entries(&display_names, &methods, &class_map);
    assert_eq!(out[0].0, "Groups.OrgTreeTests.DoThing");
    assert_eq!(out[0].1, "Tmly.Test.Groups.OrgTreeTests.DoThing");
}

/// When many list lines share a short name across several classes, distribute rows round-robin
/// so parametric cases are not all attributed to the last class (which skews folder totals).
#[test]
fn test_build_discovery_ambiguous_round_robin_parametric() {
    let mut methods = HashMap::new();
    methods.insert(
        "Dup".to_string(),
        vec![
            ("F1".to_string(), "Ns.Alpha.C1".to_string()),
            ("F1".to_string(), "Ns.Beta.C2".to_string()),
        ],
    );
    // folder F1 + qualified Ns.Alpha.C1 / Ns.Beta.C2 → tree path F1.C1.Dup / F1.C2.Dup
    let display_names = vec![
        "Dup(1)".to_string(),
        "Dup(2)".to_string(),
        "Dup(3)".to_string(),
        "Dup(4)".to_string(),
    ];
    let class_map = HashMap::new();
    let out = build_discovery_entries(&display_names, &methods, &class_map);
    let c1: usize = out
        .iter()
        .filter(|(t, _, _)| t.contains("F1.C1.Dup"))
        .map(|(_, _, c)| c)
        .sum();
    let c2: usize = out
        .iter()
        .filter(|(t, _, _)| t.contains("F1.C2.Dup"))
        .map(|(_, _, c)| c)
        .sum();
    assert_eq!(c1, 2, "round-robin: 4 lines / 2 classes");
    assert_eq!(c2, 2);
}

/// Same short method name in different classes must become separate discovery rows (distinct filters).
#[test]
fn test_build_discovery_duplicate_short_method_names_one_row_per_class() {
    let mut methods = HashMap::new();
    methods.insert(
        "SmokeTest".to_string(),
        vec![
            ("Imports".to_string(), "Tmly.Test.Imports.ImportRollupTests".to_string()),
            ("ImportLookupCluesTest".to_string(), "Tmly.Test.Imports.ImportLookupCluesTest".to_string()),
        ],
    );
    let display_names = vec!["SmokeTest".to_string(), "SmokeTest".to_string()];
    let class_map = HashMap::new();
    let out = build_discovery_entries(&display_names, &methods, &class_map);
    assert_eq!(out.len(), 2);
    assert_eq!(out.iter().map(|(_, _, c)| c).sum::<usize>(), 2);
}

#[test]
fn test_strip_params() {
    // NUnit pattern
    assert_eq!(strip_params("Namespace.Class.Method(1, 2)"), "Namespace.Class.Method");
    
    // XUnit pattern with named parameters
    assert_eq!(strip_params("Namespace.Class.Method(x: 1, s: \"val\")"), "Namespace.Class.Method");

    // Standard parameterless
    assert_eq!(strip_params("Namespace.Class.Method"), "Namespace.Class.Method");

    // Complex balanced parens with content
    assert_eq!(strip_params("Namespace.Class.Method(\"val (with paren)\")"), "Namespace.Class.Method");

    // Generic methods
    assert_eq!(strip_params("Namespace.Class.Method<int>(1)"), "Namespace.Class.Method");
    assert_eq!(strip_params("Namespace.Class.Method<string>"), "Namespace.Class.Method");
}

#[test]
fn test_test_attributes() {
    // NUnit
    assert!(is_test_attribute("[Test]"));
    assert!(is_test_attribute("[TestCase(1, 2)]"));
    assert!(is_test_attribute("[TestCase]")); // bare
    assert!(is_test_attribute("[TestCaseSource(\"src\")]"));
    assert!(is_test_attribute("[Test, Category(\"Slow\")]"));
    
    // XUnit specific
    assert!(is_test_attribute("[Fact]"));
    assert!(is_test_attribute("[Theory]"));
    
    // MSTest specific
    assert!(is_test_attribute("[TestMethod]"));
    assert!(is_test_attribute("[DataRow(1)]"));
    
    // Robustness: spaces inside brackets
    assert!(is_test_attribute("[ Test]"));
    assert!(is_test_attribute("[Test ]"));
    assert!(is_test_attribute("[ TestCase(1) ]"));
    
    // Safety
    assert!(!is_test_attribute("[Tast]")); // typo
    assert!(!is_test_attribute("Just text [Test]")); // not starting with [
}

#[test]
fn test_extract_method_name() {
    let cases = vec![
        // NUnit/XUnit Standard
        ("public void SimpleTest()", "SimpleTest"),
        ("public async Task AsyncTest(int x)", "AsyncTest"),
        ("internal static void InternalStaticTest()", "InternalStaticTest"),
        
        // NUnit Generic
        ("public void GenericTest<T>()", "GenericTest"), 
        // NUnit Generic + Params
        ("public void GenericParam<T>(T obj)", "GenericParam"),
        
        // XUnit Theory
        ("public void Test1(int i)", "Test1"),

        // Tricky spaces
        ("public  void   WeirdSpaces (  ) ", "WeirdSpaces"),
    ];

    for (line, expected) in cases {
        assert_eq!(extract_method_name(line).unwrap(), expected, "Failed on: {}", line);
    }

    // Keywords should be ignored cleanly
    assert_eq!(extract_method_name("if (true)"), None);
    assert_eq!(extract_method_name("while (x > 0)"), None);
}

#[test]
fn test_extract_class_name() {
    assert_eq!(extract_class_name("class SimpleClass").unwrap(), "SimpleClass");
    assert_eq!(extract_class_name("public class PubClass").unwrap(), "PubClass");
    assert_eq!(extract_class_name("internal sealed class SealedClass").unwrap(), "SealedClass");
    assert_eq!(extract_class_name("public abstract partial class PartialClass").unwrap(), "PartialClass");
    
    // Space spacing glitch that existed before
    assert_eq!(extract_class_name("public  class  SpacesClass  {").unwrap(), "SpacesClass");
}

#[test]
fn test_enrich_tree_generation() {
    let mut method_map = HashMap::new();
    let mut class_map = HashMap::new();

    // Scenario 1: standard setup where discovering maps correctly
    class_map.insert("LoginTests".to_string(), "Backend.Auth".to_string());
    method_map.insert("TestValidLogin".to_string(), ("Backend.Auth".to_string(), "LoginTests".to_string()));

    // NUnit & XUnit fully qualified output: MyProject.Backend.Auth.LoginTests.TestValidLogin
    let fqn = "MyProject.Backend.Auth.LoginTests.TestValidLogin";
    let enriched_fqn = enrich(fqn, &method_map, &class_map);
    // Strips namespace, keeps class+method, prepends folder
    assert_eq!(enriched_fqn, "Backend.Auth.LoginTests.TestValidLogin");

    // Scenario 2: empty folder (files at project root)
    class_map.insert("RootTests".to_string(), "".to_string());
    method_map.insert("RootMethod".to_string(), ("".to_string(), "RootTests".to_string()));

    let fqn_root = "Project.RootTests.RootMethod";
    assert_eq!(enrich(fqn_root, &method_map, &class_map), "RootTests.RootMethod");
}

#[test]
fn test_enrich_strips_namespace_for_deep_path() {
    let mut method_map = HashMap::new();
    let mut class_map = HashMap::new();

    class_map.insert("IfWorkedRuleTests".to_string(), "Conversion.Rules".to_string());
    method_map.insert("GroupingRule_Simple".to_string(), ("Conversion.Rules".to_string(), "IfWorkedRuleTests".to_string()));
    method_map.insert("IfWorkedRule_TopUpTest".to_string(), ("Conversion.Rules".to_string(), "IfWorkedRuleTests".to_string()));
    method_map.insert("FlatRate_SingleHour".to_string(), ("Conversion.Rules".to_string(), "IfWorkedRuleTests".to_string()));

    let fqn = "Tmly.Test.Conversion.Rules.IfWorkedRuleTests.GroupingRule_Simple";
    let enriched = enrich(fqn, &method_map, &class_map);

    assert_eq!(enriched, "Conversion.Rules.IfWorkedRuleTests.GroupingRule_Simple",
        "Namespace prefix should be stripped; tests should NOT appear detached");

    let fqn2 = "Tmly.Test.Conversion.Rules.IfWorkedRuleTests.FlatRate_SingleHour";
    assert_eq!(enrich(fqn2, &method_map, &class_map), "Conversion.Rules.IfWorkedRuleTests.FlatRate_SingleHour");

    let fqn3 = "Tmly.Test.Conversion.Rules.IfWorkedRuleTests.GetLookBackDate_ForTopUp_WorksOkWithTimesheetImpact";
    assert_eq!(enrich(fqn3, &method_map, &class_map),
        "Conversion.Rules.IfWorkedRuleTests.GetLookBackDate_ForTopUp_WorksOkWithTimesheetImpact");

    // Generic method in display name should match non-generic in source map
    let fqn_gen = "Tmly.Test.Conversion.Rules.IfWorkedRuleTests.GenericMethod<int>";
    method_map.insert("GenericMethod".to_string(), ("Conversion.Rules".to_string(), "IfWorkedRuleTests".to_string()));
    assert_eq!(enrich(fqn_gen, &method_map, &class_map), "Conversion.Rules.IfWorkedRuleTests.GenericMethod<int>");
}
#[test]
fn test_enrich_simple_class_dot_method() {
    let mut method_map = HashMap::new();
    let mut class_map = HashMap::new();

    class_map.insert("SimpleTests".to_string(), "Unit".to_string());
    method_map.insert("TestOne".to_string(), ("Unit".to_string(), "SimpleTests".to_string()));

    // Simple case: ClassName.Method (no namespace prefix)
    assert_eq!(enrich("SimpleTests.TestOne", &method_map, &class_map), "Unit.SimpleTests.TestOne");

    // No folder
    class_map.insert("RootTests".to_string(), "".to_string());
    assert_eq!(enrich("RootTests.TestOne", &method_map, &class_map), "RootTests.TestOne");
}

#[test]
fn test_parse_cs_content_inline_test() {
    let content = r##"
        public class MyTests {
            [Test] public void FlatRate_SingleHour() => FlatRateTest(["token: 1"], ["#1 token: 1 @ 7.00 = 7.00"]);
        }
    "##;
    let mut methods = HashMap::new();
    let mut classes = HashMap::new();
    parse_cs_content(content, "TestDir", &mut methods, &mut classes);

    assert_eq!(methods.len(), 1, "Failed to extract inline test method");
    assert!(methods.contains_key("FlatRate_SingleHour"));
}

#[test]
fn test_parse_cs_content_if_worked_rules() {
    let content = r##"
namespace Tmly.Test.Conversion.Rules;

using Shouldly;

public class IfWorkedRuleTests : ConversionRuleTests {

    [Test]
    public void GroupingRule_Simple() {
        UseRates();
    }

    [Test]
    public void IfWorkedRule_TwoPlacements_SameDay() {
        UseRates();
    }

    [Test] public void FlatRate_SingleHour() => FlatRateTest(["token: 1"], ["#1 token: 1 @ 7.00 = 7.00"]);
    [Test] public void FlatRate_MultiHour() => FlatRateTest(["token: 2"], ["#1 token: 1 @ 7.00 = 7.00"]);

    [TestCase(true)]
    [TestCase(false)]
    public void IfWorkedRule_CurrencyMarkup_CheckPerPlacement_TwoPlacements(bool perPlacement) {
        UseRates();
    }
}
"##;
    let mut methods = HashMap::new();
    let mut classes = HashMap::new();
    parse_cs_content(content, "Conversion.Rules", &mut methods, &mut classes);

    // Should find the class
    assert!(classes.contains_key("IfWorkedRuleTests"), "Should find IfWorkedRuleTests class");
    assert_eq!(classes["IfWorkedRuleTests"], "Conversion.Rules");

    // Should find all test methods
    assert!(methods.contains_key("GroupingRule_Simple"), "Should find GroupingRule_Simple");
    assert!(methods.contains_key("IfWorkedRule_TwoPlacements_SameDay"), "Should find IfWorkedRule_TwoPlacements_SameDay");
    assert!(methods.contains_key("FlatRate_SingleHour"), "Should find inline FlatRate_SingleHour");
    assert!(methods.contains_key("FlatRate_MultiHour"), "Should find inline FlatRate_MultiHour");
    assert!(methods.contains_key("IfWorkedRule_CurrencyMarkup_CheckPerPlacement_TwoPlacements"),
        "Should find TestCase-attributed method");

    // Verify folder mapping is correct for each method
    let (folder, qc) = &methods["GroupingRule_Simple"][0];
    assert_eq!(folder, "Conversion.Rules");
    assert!(qc.ends_with(".IfWorkedRuleTests"), "qualified class: {}", qc);
}

#[test]
fn test_parse_cs_content_comment_after_attribute() {
    let content = r##"
public class BreakTests {

    [Test] // https://www.somelink.com my comment[good]
    public async Task AdjustmentLines_AreIgnoredWhenValidatingBreaks() {
        // body
    }

    [TestCase(true, "tom p_A 06/27 REG $40.00")] // Adding bill tag
    [TestCase(false, "")] // Not adding the bill tag
    public async Task NotifyUserOfInactivatedPurchaseOrder(bool addBillTag, params string[] expected) {
        // body
    }

    [Test]
    public void NormalTest() { }
}
"##;
    let mut methods = HashMap::new();
    let mut classes = HashMap::new();
    parse_cs_content(content, "Integration", &mut methods, &mut classes);

    assert!(classes.contains_key("BreakTests"), "Should find BreakTests class");

    // [Test] // url-comment  =>  method on next line must still be found
    assert!(methods.contains_key("AdjustmentLines_AreIgnoredWhenValidatingBreaks"),
        "Method after [Test] // comment should NOT be detached");

    // [TestCase(...)] // comment  (two of them)  =>  method on the line after must be found
    assert!(methods.contains_key("NotifyUserOfInactivatedPurchaseOrder"),
        "Method after [TestCase] // comment should NOT be detached");

    // Plain [Test] still works
    assert!(methods.contains_key("NormalTest"),
        "Plain [Test] method should still be found");

    // All should map to the right class and folder
    for name in &["AdjustmentLines_AreIgnoredWhenValidatingBreaks",
                   "NotifyUserOfInactivatedPurchaseOrder",
                   "NormalTest"] {
        let (folder, qc) = &methods[*name][0];
        assert_eq!(folder, "Integration", "Wrong folder for {}", name);
        assert_eq!(qc, "BreakTests", "Wrong class for {}", name);
    }
}

#[test]
fn test_user_reported_count_mismatch() {
    let mut methods = HashMap::new();
    let mut classes = HashMap::new();

    let content_gqh = r##"
namespace Tmly.Test.Groups;
public class GroupQueryHandlerTest {
	[SetUp] public async Task Setup() => _scene = null;
	[TearDown] public void TearDown() => _scene?.TearDown();

	[Test] public async Task BasicFiltersByTypeAndSearchText() { }
	[Test] public async Task FiltersByBlankGroupType() { }
	[Test] public async Task QueryParam_UseBoostTest() { }
	[Test] public async Task CanLimitScopeUnderTmlyGroupId() { }

	[TestCase(null)]
	[TestCase(BaseQueryRequest.FieldOptions.All)]
	[TestCase(BaseQueryRequest.FieldOptions.Minimal)]
	public async Task QueryParam_FieldsTest(string fields) { }

	[TestCase("apple", null, null, "apple, store1, store2")]
	[TestCase("apple", "cus", null, "apple")]
	[TestCase("apple", "loc", null, "store1, store2")]
	[TestCase("apple", null, "fud", "")]
	[TestCase("store1", null, null, "store1")]
	[TestCase("store1", "cus", null, "")]
	[TestCase("store1", null, "fud", "")]
	public async Task ProperlyRestrictsByGroupBasedPermission(string permissionScope, string type, string search, string results) { }

	[TestCase("s1", null, null, "apple, store1")]
	[TestCase("s1, fudge", null, null, "apple, fudge, store1, storeA, storeB")]
	public async Task ProperlyRestrictsByPlacementBasedPermission(string permissionScope, string type, string search, string results) { }
}
"##;

    let content_group_tests = r##"
namespace Tmly.Test.Groups;
public class GroupTests {
	[TestCase("root", null, true)]
	[TestCase("root", "root", true)]
	[TestCase("g2_a", "g2_b", false)]
	[TestCase("g2_b_b_a", "g1", false)]
	[TestCase("g2_b", "g2_b_a", false)]
	[TestCase("g2_b", "root", true)]
	[TestCase("g2_a", "g2", true)]
	[TestCase("g2_b", "g2", true)]
	[TestCase("g2_b_a", "g2", true)]
	[TestCase("g2_b_b", "g2", true)]
	[TestCase("g2_b_b_a", "g2", true)]
	public async Task GroupById_CanEditValue_ReflectsAccessLevel(string targetGroup, string accessScope, bool canEdit) { }

	[Test] public async Task GroupSaveHandlerTest() { }
	[Test] public async Task GroupSaveHandler_ThrowsIfInvalidParent() { }
}
"##;

    let content_org_tree = r##"
namespace Tmly.Test.Groups;
public class OrgTreeTests {
	[TestCase(AccessType.ManageUserAccess, "g1a g1b g1c")]
	[TestCase(AccessType.AssignPayCodes, "g1 g2 g3")]
	[TestCase(AccessType.ConfigureTimeEntry, "g3aa")]
	public async Task DifferentAccessGetsRespectiveOrgTree(AccessType access, string expected) { }

	[TestCase("a", "g1a g1aa g1ba g1ca g2a g3a g3aa")]
	[TestCase("2", "g2 g2a")]
	[TestCase("g1", "g1 g1a g1aa g1b g1ba g1c g1ca")]
	[TestCase("g", "g1 g1a g1aa g1b g1ba g1c g1ca g2 g2a g3 g3a g3aa")]
	[TestCase("", "g1 g1a g1aa g1b g1ba g1c g1ca g2 g2a g3 g3a g3aa")]
	public async Task SearchThroughOrgList(string search, string expected) { }

	[TestCase(AccessType.ManageUserAccess, "g1 g1a g1aa g1b g1ba g1c g1ca")]
	[TestCase(AccessType.AssignPayCodes, "g1 g1a g1aa g1b g1ba g1c g1ca g2 g2a g3 g3a g3aa")]
	[TestCase(AccessType.ConfigureTimeEntry, "g3a g3aa")]
	public async Task OrgListAuthorizationTest(AccessType access, string expected) { }
}
"##;

    parse_cs_content(content_gqh, "GQH", &mut methods, &mut classes);
    parse_cs_content(content_group_tests, "Groups", &mut methods, &mut classes);
    parse_cs_content(content_org_tree, "Org", &mut methods, &mut classes);

    fn qc_is_class(qc: &str, simple: &str) -> bool {
        qc == simple || qc.ends_with(&format!(".{}", simple))
    }

    // Count methods per class (qualified class name in source map)
    let gqh_methods: Vec<_> = methods.iter().filter(|(_, vecs)| {
        vecs.iter().any(|(_, qc)| qc_is_class(qc, "GroupQueryHandlerTest"))
    }).collect();
    let group_methods: Vec<_> = methods.iter().filter(|(_, vecs)| {
        vecs.iter().any(|(_, qc)| qc_is_class(qc, "GroupTests"))
    }).collect();
    let org_methods: Vec<_> = methods.iter().filter(|(_, vecs)| {
        vecs.iter().any(|(_, qc)| qc_is_class(qc, "OrgTreeTests"))
    }).collect();

    assert_eq!(gqh_methods.len(), 7, "GroupQueryHandlerTest should have 7 test methods");
    assert_eq!(group_methods.len(), 3, "GroupTests should have 3 test methods");
    assert_eq!(org_methods.len(), 3, "OrgTreeTests should have 3 test methods");
}

#[test]
fn test_tree_test_count_for_parameterised_tests() {
    use crate::core::tree::build_flat_tree;

    // Simulate what discover_tests returns: (tree_fqn, filter_key, test_count)
    // - SimpleTest has 1 instance (plain [Test])
    // - ParamTest has 5 instances ([TestCase] x5)
    // - AnotherTest has 3 instances
    let tests = vec![
        ("Folder.MyClass.SimpleTest".to_string(), "Ns.Folder.MyClass.SimpleTest".to_string(), 1),
        ("Folder.MyClass.ParamTest".to_string(), "Ns.Folder.MyClass.ParamTest".to_string(), 5),
        ("Folder.MyClass.AnotherTest".to_string(), "Ns.Folder.MyClass.AnotherTest".to_string(), 3),
    ];

    let tree = build_flat_tree(&tests);

    // Should have: Folder (non-leaf) -> MyClass (non-leaf) -> 3 leaves
    let leaves: Vec<_> = tree.iter().filter(|n| n.is_leaf).collect();
    assert_eq!(leaves.len(), 3, "Should have 3 leaf nodes (one per method)");

    // Total test_count across all leaves should be 1+5+3 = 9
    let total: usize = leaves.iter().map(|n| n.test_count).sum();
    assert_eq!(total, 9, "Total test_count should be 9 (sum of all parameterised variants)");

    // Verify individual counts
    let simple = leaves.iter().find(|n| n.label == "SimpleTest").unwrap();
    assert_eq!(simple.test_count, 1);

    let param = leaves.iter().find(|n| n.label == "ParamTest").unwrap();
    assert_eq!(param.test_count, 5);

    let another = leaves.iter().find(|n| n.label == "AnotherTest").unwrap();
    assert_eq!(another.test_count, 3);

    // Non-leaf nodes should have test_count = 0
    let non_leaves: Vec<_> = tree.iter().filter(|n| !n.is_leaf).collect();
    for node in &non_leaves {
        assert_eq!(node.test_count, 0, "Non-leaf '{}' should have test_count=0", node.label);
    }
}

/// Tests the new FQN-aware matching logic in build_discovery_entries.
/// This ensures that NUnit-style fully qualified display names are correctly matched to source
/// methods even when namespaces are complex.
#[test]
fn test_build_discovery_fqn_aware_matching() {
    let mut methods = HashMap::new();
    // Source parsing found: Placement_Primary in Tmly.Test.Infrastructure.BaseQueryHelperTests
    methods.insert(
        "Placement_Primary".to_string(),
        vec![("Infrastructure".to_string(), "Tmly.Test.Infrastructure.BaseQueryHelperTests".to_string())],
    );
    
    // dotnet test -t returned the FQN
    let display_names = vec![
        "Tmly.Test.Infrastructure.BaseQueryHelperTests.Placement_Primary".to_string(),
    ];
    
    let class_map = HashMap::new(); // Not needed for FQN match
    let out = build_discovery_entries(&display_names, &methods, &class_map);
    
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, "Infrastructure.BaseQueryHelperTests.Placement_Primary", "Should use correct folder-prefixed tree FQN");
    assert_eq!(out[0].1, "Tmly.Test.Infrastructure.BaseQueryHelperTests.Placement_Primary", "Filter key should be the FQN");
}
