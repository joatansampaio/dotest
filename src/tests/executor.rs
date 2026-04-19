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
    // Prepends directory cleanly
    assert_eq!(enriched_fqn, "Backend.Auth.MyProject.Backend.Auth.LoginTests.TestValidLogin");

    // Scenario 2: empty folder (files at project root)
    class_map.insert("RootTests".to_string(), "".to_string());
    method_map.insert("RootMethod".to_string(), ("".to_string(), "RootTests".to_string()));

    let fqn_root = "Project.RootTests.RootMethod";
    assert_eq!(enrich(fqn_root, &method_map, &class_map), "Project.RootTests.RootMethod");
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
