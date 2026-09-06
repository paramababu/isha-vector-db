// Autolinking, stated rather than inferred.
//
// React Native's CLI can usually work all of this out by convention. Saying it explicitly costs
// nothing and removes the two ambiguities that actually bite: which podspec, when a package has
// more than one file that could be, and which class to register, when the CLI's source scan has
// to guess from a package with several.

module.exports = {
  dependency: {
    platforms: {
      android: {
        sourceDir: 'android',
        packageImportPath: 'import dev.isha.vectordb.reactnative.VdbPackage;',
        packageInstance: 'new VdbPackage()',
      },
      ios: {
        podspecPath: __dirname + '/isha-vector-db.podspec',
      },
    },
  },
};
