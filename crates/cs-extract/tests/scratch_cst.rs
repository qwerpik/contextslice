//! TEMPORARY scratch: dump CSTs of Go constructs to verify node shapes before
//! freezing queries. Deleted before the milestone commit.

use cs_extract::parse_source;
use cs_scanner::Language;

fn dump(name: &str, src: &str) {
    let (tree, status) = parse_source(src, Language::Go).expect("parse");
    println!("=== {name} [{status:?}]\n{}\n", tree.root_node().to_sexp());
}

#[test]
fn dump_csts() {
    dump(
        "grouped consts with docs",
        "package p\n\nconst (\n\t// A docs\n\tA = 1\n\t// B docs\n\tB = 2\n)\n",
    );
    dump(
        "grouped types",
        "package p\n\ntype (\n\t// T docs\n\tT struct{ X int }\n\tU = int\n)\n",
    );
    dump(
        "grouped vars",
        "package p\n\nvar (\n\tX int\n\tY, Z string\n)\n",
    );
    dump(
        "imports",
        "package p\n\nimport (\n\t\"fmt\"\n\tf \"fmt\"\n\t. \"math\"\n\t_ \"embed\"\n)\n",
    );
    dump(
        "method + receiver",
        "package p\n\ntype S struct{}\n\nfunc (s *S) Get() int { return 0 }\n",
    );
    dump(
        "interface",
        "package p\n\ntype I interface {\n\t// M docs\n\tM(x int) error\n\tio.Writer\n}\n",
    );
    dump(
        "embedded struct",
        "package p\n\ntype A struct{ N int }\ntype B struct {\n\tA\n\tX int `json:\"x\"`\n}\n",
    );
    dump("generics", "package p\n\ntype Pair[T any] struct{ L, R T }\nfunc Map[T, U any](in []T, f func(T) U) []U { return nil }\n");
    dump("calls and selectors", "package p\n\nfunc F() {\n\tx := pkg.Call(a.B, c.D.E)\n\tm.Field = 7\n\t_ = w.Write(b)\n}\n");
    dump(
        "composite literal",
        "package p\n\ntype P struct{ Name string }\nfunc F() { _ = P{Name: \"x\", Other: 1} }\n",
    );
    dump("types in params", "package p\n\ntype S struct{}\nfunc F(s *S, m map[string]S, f func(S) S) (S, error) { return s, nil }\n");
    dump("short var decl", "package p\n\nfunc F() {\n\tx, err := g()\n\tif err != nil { return }\n\tfor i, v := range xs { _ = i; _ = v }\n\t_ = x\n}\n");
    dump("local decls in func", "package p\n\nfunc F() {\n\tconst localC = 1\n\ttype localT struct{}\n\tvar localV int\n}\n");
    dump("labels go defer select", "package p\n\nfunc F() {\nLoop:\n\tfor {\n\t\tselect {\n\t\tcase <-ch:\n\t\t\tbreak Loop\n\t\t}\n\t}\n\tdefer close(ch)\n\tgo F()\n}\n");
    dump(
        "func literal var",
        "package p\n\nvar handler = func(e Event) error { return nil }\nvar _ = func() {}\n",
    );
    dump(
        "dot import usage",
        "package p\n\nimport . \"math\"\n\nfunc F() float64 { return Sqrt(2) }\n",
    );
    dump(
        "unicode export",
        "package p\n\nfunc Ünicode() int { return 0 }\nfunc ünicode() int { return 1 }\n",
    );
}
