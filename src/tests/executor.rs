use std::collections::HashMap;
use crate::core::executor::{strip_params, is_test_attribute, extract_method_name, extract_class_name, enrich, parse_cs_content};

#[test]
fn test_strip_params() {
    // NUnit pattern
    assert_eq!(strip_params("Namespace.Class.Method(1, 2)"), "Namespace.Class.Method");
    
    // XUnit pattern with named parameters
    assert_eq!(strip_params("Namespace.Class.Method(x: 1, s: \"val\")"), "Namespace.Class.Method");

    // Standard parameterless
    assert_eq!(strip_params("Namespace.Class.Method"), "Namespace.Class.Method");
}

#[test]
fn test_test_attributes() {
    // NUnit
    assert!(is_test_attribute("[Test]"));
    assert!(is_test_attribute("[TestCase(1, 2)]"));
    assert!(is_test_attribute("[Test, Category(\"Slow\")]"));
    
    // XUnit specific
    assert!(is_test_attribute("[Fact]"));
    assert!(is_test_attribute("[Theory]"));
    
    // MSTest specific
    assert!(is_test_attribute("[TestMethod]"));
    
    // Safety
    assert!(!is_test_attribute("[Tast]")); // typo
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
    let (folder, class) = &methods["GroupingRule_Simple"];
    assert_eq!(folder, "Conversion.Rules");
    assert_eq!(class, "IfWorkedRuleTests");
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
        let (folder, class) = &methods[*name];
        assert_eq!(folder, "Integration", "Wrong folder for {}", name);
        assert_eq!(class, "BreakTests", "Wrong class for {}", name);
    }
}
