use viso_ende::*;

#[derive(EnBin, DeBin, EnJson, DeJson, PartialEq)]
struct MyStruct<T> where T: Clone {
    pub a: T,
    b: u32,
    c: Option<Vec<u32>>,
    d: Option<Vec<u32>>,
    e: MyEnum<T>,
    f: MyEnum<T>,
    g: MyEnum<T>,
    h: MyEnum<T>,
    i: MyEnum<T>,
    j: String,
    k: [u32;2]
}

#[derive(EnJson, DeJson, PartialEq, Debug)]
struct RenamedStruct {
    #[rename(firstName)]
    first_name: String,
    #[rename(lastName)]
    last_name: String,
    age: u32,
}

#[derive(EnBin, DeBin, EnJson, DeJson, PartialEq)]
enum MyEnum<T> where T: Clone {
    One,
    Two(T, u32),
    Three {x: u32, y: T},
    Four {z: Option<u32>, w: T},
}

fn main() {
    let x = MyStruct {
        a: 1,
        b: 2,
        c: Some(vec![3]),
        d: None,
        e: MyEnum::One,
        f: MyEnum::Two(4, 5),
        g: MyEnum::Three {x: 6, y: 7},
        h: MyEnum::Four {z: None, w: 8},
        i: MyEnum::Four {z: Some(9), w: 8},
        j: "Hello".to_string(),
        k: [10,11]
    };
    let bin = x.encode_bin();
    println!("Bin len: {}", bin.len());
    let y:MyStruct<usize> = DeBin::decode_bin(&bin).unwrap();
    println!("Bin roundtrip equality {}", x == y);

    let json = x.encode_json();
    println!("JSON Output {}", json);
    let y:MyStruct<usize> = DeJson::decode_json(&json).unwrap();
    println!("JSON roundtrip equality {}", x == y);

    // Test #[rename] attribute
    let renamed = RenamedStruct {
        first_name: "John".to_string(),
        last_name: "Doe".to_string(),
        age: 30,
    };
    let renamed_json = renamed.encode_json();
    println!("\nRenamed JSON Output: {}", renamed_json);

    let renamed_back: RenamedStruct = DeJson::decode_json(&renamed_json).unwrap();
    println!("Renamed roundtrip equality: {}", renamed == renamed_back);

    let external_json = r#"{"firstName":"Jane","lastName":"Smith","age":25}"#;
    let from_external: RenamedStruct = DeJson::decode_json(external_json).unwrap();
    println!("decoded from external JSON: {:?}", from_external);
}
